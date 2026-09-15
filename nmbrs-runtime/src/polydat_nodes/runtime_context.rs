// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Runtime context nodes (SRD 12 §"Runtime context nodes").
//!
//! These nodes project stable runtime surfaces — the current
//! phase, the current cycle ordinal, the value of a dynamic
//! control, the active rate-limiter target, the active fiber
//! count — into GK-readable wires. They are the read-side of
//! the reification principle (SRD 10 §"GK as the unified access
//! surface"): any value a workload might want to read is reached
//! through a Polydat binding, not a side channel.
//!
//! Like the metric nodes (see `metrics.rs`), these are
//! non-deterministic context projections — their output changes
//! between cycles by definition, so the constant-folder is not
//! allowed to collapse them. They read from globals / thread
//! locals the runtime sets during bootstrap and on every cycle
//! tick.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

// All nodes here are authored via `#[polydat::polydat_node]` (fully-qualified
// `polydat::…` paths), so no `polydat::ast` imports are needed in module
// code (the in-module tests import what they need locally).

use nmbrs_metrics::component::Component;

// =========================================================================
// Global session-root handle + per-fiber task-local context
// =========================================================================

/// Global handle to the session's component root. Set by the
/// runner during scenario bootstrap so context nodes can resolve
/// `control(...)` reads against the live tree.
static SESSION_ROOT: LazyLock<Mutex<Option<Arc<RwLock<Component>>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Install the session root for every runtime-context node that
/// reads tree state. Call once at scenario bootstrap; subsequent
/// calls overwrite.
/// Test-only serialization for the process-global session root.
///
/// `SESSION_ROOT` is one global; the parallel test runner is many threads.
/// Every test that INSTALLS a root — directly, or by constructing a
/// [`crate::session::Session`], whose constructor installs one as a side
/// effect — must hold this guard, and so must every test that READS controls
/// through the root. It lives here, beside the global it protects, precisely
/// because the offender that motivated it was in another module: a
/// session-construction test stomping the control tests' root through the
/// constructor side effect, which a module-private test lock could never
/// exclude.
#[cfg(test)]
pub(crate) fn session_root_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_session_root(root: Arc<RwLock<Component>>) {
    *SESSION_ROOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(root);
}

fn session_root() -> Option<Arc<RwLock<Component>>> {
    SESSION_ROOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Public accessor for the runner-installed session root.
///
/// Used by integration layers that need to walk the component
/// tree from outside this crate — the TUI's control-edit
/// handler, the web API's control endpoints, the
/// `dryrun=controls` renderer, etc. Returns `None` when no
/// session is running (pre-bootstrap, after teardown, tests
/// that never called [`set_session_root`]).
pub fn session_root_handle() -> Option<Arc<RwLock<Component>>> {
    session_root()
}

/// Per-fiber execution context carried across the async call
/// chain via a [`tokio::task_local!`] binding. Thread-locals are
/// unsafe here because tokio's work-stealing scheduler can
/// migrate a task between worker threads at any `.await`; a
/// task-local is bound to the task itself and survives
/// migration.
pub struct FiberContext {
    /// Name of the phase this fiber is running under. Arc'd so
    /// two fibers under the same phase share one allocation.
    pub phase: Arc<str>,
    /// Current cycle ordinal. `AtomicU64` rather than `Cell` so
    /// the context is `Sync` — tokio requires task-local values
    /// to be `Send + Sync`.
    pub cycle: AtomicU64,
    /// SRD-89 — the **current component's** control resolution, for this
    /// fiber's execution: control name → live erased handle, walked up **once**
    /// from the fiber's own phase component
    /// ([`nmbrs_metrics::component::Component::control_snapshot`], first-match /
    /// nearest-wins) and read lock-free thereafter. It holds live handles, not
    /// values, so a servo retarget on the shared control is observed through it.
    /// Because each execution's fibers carry **their own** phase component's
    /// snapshot, a same-named control (`concurrency` / `rate`) resolves to that
    /// execution's instance — uniformly in single-run and concurrent runs, with
    /// no session-root walk and no cross-talk. Empty for fibers spawned without
    /// a component (a control read then falls back to the session-root walk).
    pub controls: ControlMap,
}

/// A lock-free, shareable snapshot of resolved control handles (see
/// [`FiberContext::controls`]).
pub type ControlMap =
    Arc<std::collections::HashMap<String, Arc<dyn nmbrs_metrics::controls::ErasedControl>>>;

/// Snapshot the controls visible from `component` (its own controls plus any
/// `Subtree`-scoped ancestor control, nearest-wins) into a lock-free
/// [`ControlMap`]. Deadlock-safe: [`Component::control_snapshot`] acquires one
/// tier's lock at a time, never nested — so it is safe on the cadence-contended
/// component tree. Computed once per phase, shared across the phase's fibers.
pub fn snapshot_controls(component: &Arc<RwLock<Component>>) -> ControlMap {
    Arc::new(nmbrs_metrics::component::Component::control_snapshot(
        component,
    ))
}

/// An empty [`ControlMap`] for fibers / call sites that have no component (the
/// read then falls back to the session-root walk).
pub fn empty_controls() -> ControlMap {
    Arc::new(std::collections::HashMap::new())
}

tokio::task_local! {
    /// Task-local fiber context. Set once per fiber at spawn
    /// time via [`with_fiber_context`]; updated per cycle via
    /// [`set_task_cycle`]. Reads outside a scope (e.g. unit
    /// tests, non-fiber code paths) silently return defaults.
    static FIBER_CTX: FiberContext;
}

/// Wrap a fiber's async body in a `FiberContext` scope. Every
/// runtime-context node read performed inside the future sees
/// the phase name and cycle counter set here.
///
/// Cycle starts at 0 and is updated via [`set_task_cycle`] on
/// every iteration of the fiber's loop.
pub async fn with_fiber_context<F>(phase: Arc<str>, controls: ControlMap, fut: F) -> F::Output
where
    F: Future,
{
    FIBER_CTX
        .scope(
            FiberContext {
                phase,
                cycle: AtomicU64::new(0),
                controls,
            },
            fut,
        )
        .await
}

/// Resolve a control from the running fiber's **current component** snapshot
/// (lock-free). `None` outside a fiber scope, or when the snapshot doesn't carry
/// `name` — the caller then falls back to the session-root walk.
fn current_phase_control(name: &str) -> Option<Arc<dyn nmbrs_metrics::controls::ErasedControl>> {
    FIBER_CTX
        .try_with(|ctx| ctx.controls.get(name).cloned())
        .ok()
        .flatten()
}

/// Update the cycle counter in the enclosing [`FiberContext`].
/// Safe to call outside a scope — the update is a no-op if
/// there is no active fiber context (e.g. when the node is
/// evaluated from a unit test that didn't install one).
pub fn set_task_cycle(cycle: u64) {
    let _ = FIBER_CTX.try_with(|ctx| ctx.cycle.store(cycle, Ordering::Relaxed));
}

fn task_phase() -> Option<Arc<str>> {
    FIBER_CTX.try_with(|ctx| ctx.phase.clone()).ok()
}

fn task_cycle() -> u64 {
    FIBER_CTX
        .try_with(|ctx| ctx.cycle.load(Ordering::Relaxed))
        .unwrap_or(0)
}

// =========================================================================
// control_set(name, value) — GK-driven write into a control
// =========================================================================

/// Capture the enclosing DSL binding name at node construction, for write
/// attribution (`ControlOrigin::Polydat { binding }` — surfaces in control
/// logs so operators attribute a change to a specific binding, not just
/// "from GK"). The DSL compiler installs the current binding into the
/// compile context before each builder runs; falls back to the control name
/// when no binding scope is active (e.g. a library test).
fn capture_binding(name: &str) -> String {
    polydat::dsl::factory::compile_ctx::current_binding().unwrap_or_else(|| name.to_string())
}

/// Polydat write node: submit an f64 write against the named control via the
/// session root's walk-up. Returns `1` if dispatched, `0` if not — either no
/// session root is installed (outside a running scenario / pure-kernel test)
/// or the control already reads exactly the requested value (idempotent
/// writes are elided; see the fixpoint check in the body).
///
/// Signature: `control_set(name: const str, value: f64) -> u64`. Authored
/// via `#[polydat::polydat_node]` (SRD-80b).
///
/// Writes are **non-blocking** — the node spawns a tokio task that calls
/// [`nmbrs_metrics::controls::ErasedControl::set_f64`] and does not await it
/// (awaiting would deadlock the single-threaded runtime). The control
/// layer's confirmed-apply still runs in the background; failures log but do
/// not stall the issuing fiber. SRD 23 §"Mutation entry points →
/// GK-driven feedback loops".
///
/// Purity is `SideChannel(Other)`: the write is an observable side effect on
/// runtime control state, so the node is never const-folded or deduped — it
/// dispatches on every evaluation.
#[polydat::polydat_node(
    category = Context,
    purity = SideChannel(Other),
)]
fn control_set(
    name: Const<&str>,
    #[poly_const(capture_binding, from = name)] binding: &String,
    value: f64,
) -> u64 {
    // SRD-89 — a workload's `control_set(...)` must write to ITS OWN phase-tier
    // control (`concurrency` / `rate`), not a neighbour's. Resolve through the
    // SAME path as the read nodes ([`resolve_control`]: the running fiber's
    // current-component snapshot, else the session-root walk-up) so the write
    // targets exactly the control a subsequent read sees. Resolution happens
    // HERE, inside the fiber's `FIBER_CTX` scope — the background write task
    // below does NOT inherit `FIBER_CTX`, so it cannot re-resolve; we capture
    // the live handle and hand it over. If nothing resolves (pre-bootstrap /
    // pure-kernel test) report "not dispatched" (the no-root → 0 contract)
    // without spawning.
    let Some(erased) = resolve_control(name.0) else {
        return 0;
    };
    // Idempotent-write elision (SRD-93-era servo support): a feedback
    // binding that re-evaluates per cycle recomputes the SAME target
    // between changes of its inputs, and re-dispatching that write
    // buys nothing while costing a spawn + the confirmed-apply
    // pipeline every cycle. The committed gauge projection is the
    // fixpoint check: when the control already reads exactly the
    // requested value, the write is elided and the node reports 0
    // (not dispatched — same code as the no-root case). Exact
    // equality on purpose: between input changes the recomputation is
    // bit-identical, and any real change, however small, must
    // dispatch. Controls without a reified gauge (`gauge_f64() ==
    // None`) never match and keep the always-dispatch behavior. An
    // in-flight uncommitted write can let one duplicate through —
    // harmless, it commits the same value.
    if erased.gauge_f64() == Some(value) {
        return 0;
    }
    let name = name.0.to_string();
    let binding = binding.clone();

    // Dispatch the write on a background task — the fiber cannot await an
    // async `set` without blocking the runtime. The handle is already resolved,
    // so the task needs no execution context.
    tokio::spawn(async move {
        let origin = nmbrs_metrics::controls::ControlOrigin::Polydat { binding };
        if let Err(e) = erased.set_f64(value, origin).await {
            polydat::audit::warn(&format!("control_set({name}, {value}) failed: {e}"));
        }
    });

    1
}

// =========================================================================
// control(name) — read a dynamic control's gauge-projection
// =========================================================================

/// Read the current committed value of a dynamic control as an
/// `f64`, projected through the control's reified gauge.
///
/// Signature: `control(name: String) -> f64`
///
/// Resolves by walking up the component tree from the session
/// root for the first declaration that matches the given name
/// (honoring branch scope). Returns `0.0` in every case where
/// no numeric value is available:
///
/// - Name doesn't resolve to any declared control.
/// - Control is declared but was built without
///   [`nmbrs_metrics::controls::ControlBuilder::reify_as_gauge`],
///   i.e. no f64 projection was registered.
/// - Control has a projection but the current value's
///   `to_f64` returned `None` (e.g. an enum-valued control
///   sitting on a `None`-projected variant).
///
/// This is deliberately silent rather than error-raising: a
/// workload running at cycle time cannot usefully "handle" a
/// missing control, and the alternative (panicking or
/// returning a sentinel like `NaN`) propagates poison
/// downstream. For typed reads of non-numeric controls use
/// [`ControlStr`] (`control_str`) / [`ControlBool`]
/// (`control_bool`), which have explicit defaults for the same
/// missing-control cases. Operators verify controls exist via
/// `dryrun=controls` before running.

/// Resolve a control's erased handle for a cycle-time read or write (SRD-89).
///
/// **Per-execution first, lock-free:** consults the current execution's
/// per-phase control map (`execution_context::current_control`) — an `ArcSwap`
/// load + `HashMap` get, touching no component lock and walking no tree. Under
/// concurrent in-process executions (SRD-88) this is what makes a same-named
/// control (`concurrency` / `rate`) resolve to THIS execution's own instance,
/// so a neighbour's servo can't drive this phase (the SRD-89 §2b cross-talk).
///
/// **Fallback — the global `SESSION_ROOT` walk:** when there is no scoped
/// execution (single-run / dryrun / TUI control-edit), no map yet, or a
/// session-tier control not in the per-exec map, resolve by walking up from the
/// installed session root via [`Component::find_control_erased_up`]. This is the
/// pre-SRD-89 path, so single-run output is byte-identical (axiom A1). The walk
/// holds a single, non-nested read guard on the session root (which has no
/// parent), released before the caller projects the value.
fn resolve_control(name: &str) -> Option<Arc<dyn nmbrs_metrics::controls::ErasedControl>> {
    // Resolve from the running fiber's **current component** — its phase-tier
    // control snapshot (walk-up from its own phase component, first match), read
    // lock-free. Uniform for single-run and concurrent: each execution's fibers
    // carry their own snapshot, so a same-named control resolves to that
    // execution's instance. No session-root start.
    if let Some(handle) = current_phase_control(name) {
        return Some(handle);
    }
    // Fallback: no fiber scope (a control edit from OUTSIDE a running phase —
    // the TUI `e` prompt, the web control endpoint, `dryrun=controls`). Walk up
    // from the session root for session-tier controls.
    let root = session_root()?;
    let guard = root.read().ok()?;
    guard.find_control_erased_up(name)
}

/// Read a dynamic control's current reified-gauge value as `f64`. `0.0` when
/// the control can't be resolved (no session / per-exec context, absent) or has
/// no reified gauge. The single read path for every control-reader node
/// (`control` / `control_u64` / `control_bool` / `rate` / `concurrency`).
fn control_gauge_f64(name: &str) -> f64 {
    resolve_control(name)
        .and_then(|c| c.gauge_f64())
        .unwrap_or(0.0)
}

/// Read a dynamic control's current value as its human-readable string
/// rendering (the erased-control `value_string()`), or `""` when absent.
fn control_value_string(name: &str) -> String {
    resolve_control(name)
        .map(|c| c.value_string())
        .unwrap_or_default()
}

/// Read a dynamic control's current reified-gauge value as `f64`, by
/// walk-up from the live session root. `0.0` when there is no session root,
/// the control is absent, or it has no reified gauge.
///
/// Signature: `control(name: const str) -> f64`. Intrinsically
/// `Nondeterministic`: the control value changes over the run (operator
/// edits, SRD-86 servo retargets, per-coordinate reruns), so the node is
/// never const-folded, re-evaluated on every pull, and the compiler
/// propagates that volatility to every downstream wire — `load :=
/// control("concurrency")` (and anything reading `load`) tracks the live
/// value without the author flagging `volatile`.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads a live dynamic control value; changes over the run"),
)]
fn control(name: Const<&str>) -> f64 {
    control_gauge_f64(name.0)
}

// =========================================================================
// control_u64(name) / control_str(name) — typed read sugar
// =========================================================================

/// Read a dynamic control's current value and cast to `u64`.
///
/// Signature: `control_u64(name: const str) -> u64`. Resolves the control by
/// walk-up from the session root, reads its reified-gauge f64 projection and
/// casts to u64 (saturating at 0 for negatives). Missing / gauge-less
/// controls return 0. For integer-valued controls (`concurrency`,
/// `max_retries`) read as a cycle-time parameter — no need to pipe
/// `control("…")` through `f64_to_u64`.
///
/// Authored via `#[polydat::polydat_node]` (SRD-80b) — purity is the
/// hygienic attribute below. `Nondeterministic` because the control value
/// changes over the run (operator edits, SRD-86 servo retargets,
/// per-coordinate reruns): the node is never const-folded, re-evaluated on
/// every pull, and the compiler propagates that volatility to every
/// downstream wire, so `load := control_u64("concurrency")` tracks the live
/// value without the author flagging `volatile`.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads a live dynamic control value; changes over the run"),
)]
fn control_u64(name: Const<&str>) -> u64 {
    let v = control_gauge_f64(name.0);
    if v < 0.0 { 0 } else { v as u64 }
}

/// Read a dynamic control's current value as a boolean — `true` iff its
/// reified-gauge value is non-zero. Missing / unreified controls → `false`.
///
/// Signature: `control_bool(name: const str) -> bool`. Intrinsically
/// `Nondeterministic` (live control read) — see [`control_u64`].
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads a live dynamic control value; changes over the run"),
)]
fn control_bool(name: Const<&str>) -> bool {
    control_gauge_f64(name.0) != 0.0
}

/// Read a dynamic control's current value as its human-readable string
/// (the erased-control `value_string()` rendering). Missing controls → `""`.
///
/// Signature: `control_str(name: const str) -> str`. Intrinsically
/// `Nondeterministic` (live control read) — see [`control_u64`].
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads a live dynamic control value; changes over the run"),
)]
fn control_str(name: Const<&str>) -> String {
    control_value_string(name.0)
}

// =========================================================================
// rate() / concurrency() — thin aliases over control(...)
// =========================================================================

/// Sugar for `control("rate")` — the current rate-limiter target (ops/sec).
/// Intrinsically `Nondeterministic` (live control read) — see [`control`].
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads the live rate control; changes over the run"),
)]
fn rate() -> f64 {
    control_gauge_f64("rate")
}

/// Sugar for `control("concurrency")` — the current fiber count for the
/// nearest phase. Intrinsically `Nondeterministic` — see [`control`].
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads the live concurrency control; changes over the run"),
)]
fn concurrency() -> f64 {
    control_gauge_f64("concurrency")
}

// =========================================================================
// phase() — current phase name (thread-local)
// =========================================================================

/// Current phase name. Reads a thread-local set by the phase executor;
/// `""` when unset (outside a cycle, or in tests that install no phase).
///
/// Intrinsically `Nondeterministic`: the per-fiber phase name varies across
/// fibers and over the run, so the node is never const-folded.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads the per-fiber phase name; varies across fibers and over the run"),
)]
fn phase() -> String {
    task_phase().map(|s| s.to_string()).unwrap_or_default()
}

// =========================================================================
// phase_start_millis() / phase_elapsed_millis() — phase-scoped clock
// =========================================================================

/// Epoch millis at which the CURRENT phase started; `0` outside a phase body.
///
/// The phase-scoped counterpart to `session_start_millis()`. Both exist because
/// they answer different questions and a session origin cannot substitute for a
/// phase one in a run that sweeps many phases.
///
/// The origin is established once per phase by the executor's `run_phase` and
/// carried across fiber spawns, so every op under the phase reads the same
/// value — including concurrent sibling phases, which each see their own
/// (the origin is a task-local, not shared execution state).
///
/// Intrinsically `Nondeterministic`: the origin differs per phase and per run,
/// so the node must never be const-folded into one phase's value.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads the current phase's start time; differs per phase and per run"),
)]
fn phase_start_millis() -> u64 {
    crate::execution_context::current_phase_start_ms().unwrap_or(0)
}

/// Milliseconds elapsed since the CURRENT phase started; `0` outside a phase.
///
/// `current_epoch_millis() - phase_start_millis()` spelled as one node, which
/// is the form workloads actually want — a phase-relative duration available
/// while the phase is still running, rather than only at its completion pull.
///
/// Intrinsically `Nondeterministic`: it grows monotonically within a phase.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("monotonic elapsed time within the current phase"),
)]
fn phase_elapsed_millis() -> u64 {
    let Some(start) = crate::execution_context::current_phase_start_ms() else {
        return 0;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_sub(start)
}

// =========================================================================
// cycle() — current cycle ordinal (thread-local)
// =========================================================================

/// Current cycle ordinal. Reads a thread-local set by the phase executor.
/// For bindings that already declare `cycle` as a named input this is
/// redundant; it exists so bindings which never named `cycle` explicitly can
/// still reach it (SRD 10's "cycle is not magic" rule — the node is context,
/// not a privileged input).
///
/// Intrinsically `Nondeterministic`: the cycle ordinal changes every cycle,
/// so the node is never const-folded.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads the per-fiber cycle ordinal; changes every cycle"),
)]
fn cycle() -> u64 {
    task_cycle()
}

// =========================================================================
// Registration
// =========================================================================
//
// Every node in this module is authored via `#[polydat::polydat_node]`
// (SRD-80b) — the control readers (`control` / `control_u64` /
// `control_bool` / `control_str` / `rate` / `concurrency`), the fiber-context
// readers (`phase` / `cycle`), and the `control_set` writer. Each macro
// emits its own FuncSig + builder + `inventory::submit!`, so there is no
// hand-written `signatures()` / `build_node()` / `register_nodes!` here.

#[cfg(test)]
mod tests {
    // The async tests hold the `serial_test()` guard (a pure `Mutex<()>`)
    // across `.await` to serialize global session-root installs; the
    // awaited code never locks it, so there's no deadlock.
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use nmbrs_metrics::controls::{BranchScope, ControlBuilder};
    use nmbrs_metrics::labels::Labels;
    use polydat::ast::Value;
    use std::collections::HashMap;
    use std::sync::MutexGuard;

    /// Serializes test access to the global `SESSION_ROOT` and the
    /// thread-locals — the crate-wide guard hoisted next to the global
    /// itself, so session-construction tests in OTHER modules can hold the
    /// same lock (see `session_root_test_guard`).
    fn serial_test() -> MutexGuard<'static, ()> {
        super::session_root_test_guard()
    }

    /// Build a session root with a declared control, install it
    /// as the global, and return the root handle so callers can
    /// mutate the control further.
    fn install_session_with_control(name: &str, initial: u32) -> Arc<RwLock<Component>> {
        let root = Component::root(Labels::empty().with("session", "t"), HashMap::new());
        root.read().unwrap().controls().declare(
            ControlBuilder::new(name, initial)
                .reify_as_gauge(|v| Some(*v as f64))
                .branch_scope(BranchScope::Subtree)
                .build(),
        );
        set_session_root(root.clone());
        root
    }

    #[test]
    fn control_reads_current_value() {
        let _g = serial_test();
        install_session_with_control("rate", 500);
        let mut k = polydat::dsl::compile_polydat("x := control(\"rate\")").expect("compile");
        assert_eq!(k.pull("x").as_f64(), 500.0);
    }

    #[test]
    fn control_missing_name_returns_zero() {
        let _g = serial_test();
        install_session_with_control("rate", 500);
        let mut k =
            polydat::dsl::compile_polydat("x := control(\"not_declared\")").expect("compile");
        assert_eq!(k.pull("x").as_f64(), 0.0);
    }

    // Live re-read after a write is covered end-to-end by
    // `fiber_writes_control_via_control_set_and_reads_back` (the integration
    // test, which yields for the async commit then re-pulls `control(...)`);
    // `control_u64_is_volatile_not_const_folded` covers the const-fold
    // property here. A unit test that pulls immediately after an async write
    // would race the background commit, so it lives at the integration tier.

    #[test]
    fn rate_node_is_alias_of_control_rate() {
        let _g = serial_test();
        install_session_with_control("rate", 750);
        let mut k = polydat::dsl::compile_polydat("x := rate()").expect("compile");
        assert_eq!(k.pull("x").as_f64(), 750.0);
    }

    #[test]
    fn concurrency_node_reads_concurrency_control() {
        let _g = serial_test();
        install_session_with_control("concurrency", 32);
        let mut k = polydat::dsl::compile_polydat("x := concurrency()").expect("compile");
        assert_eq!(k.pull("x").as_f64(), 32.0);
    }

    #[tokio::test]
    async fn phase_and_cycle_read_from_task_locals() {
        let phase_arc: Arc<str> = Arc::from("rampup");
        with_fiber_context(phase_arc.clone(), empty_controls(), async {
            set_task_cycle(4242);
            let mut k = polydat::dsl::compile_polydat("p := phase()\nc := cycle()")
                .expect("compile phase/cycle");
            assert_eq!(k.pull("p").as_str(), "rampup");
            assert_eq!(k.pull("c").as_u64(), 4242);
        })
        .await;
    }

    #[test]
    fn phase_is_empty_outside_fiber_scope() {
        // Reading outside a fiber context — e.g. from a unit test or a
        // non-fiber call site — silently returns the empty string rather
        // than panicking (the task_local's `try_with` Err maps to default).
        let mut k = polydat::dsl::compile_polydat("p := phase()").expect("compile");
        assert_eq!(k.pull("p").as_str(), "");
    }

    #[test]
    fn cycle_is_zero_outside_fiber_scope() {
        let mut k = polydat::dsl::compile_polydat("c := cycle()").expect("compile");
        assert_eq!(k.pull("c").as_u64(), 0);
    }

    #[tokio::test]
    async fn set_task_cycle_is_noop_outside_scope() {
        // A stray call with no active scope must not panic.
        set_task_cycle(99);
        let mut k = polydat::dsl::compile_polydat("c := cycle()").expect("compile");
        assert_eq!(k.pull("c").as_u64(), 0);
    }

    // ---- control_set ------------------------------------------

    #[tokio::test]
    async fn control_set_writes_through_converter_and_reaches_committed() {
        let _g = serial_test();
        // Install a root with a concurrency control that accepts
        // f64 writes via an explicit from_f64 converter.
        let root = Component::root(
            Labels::empty().with("session", "s_cs"),
            std::collections::HashMap::new(),
        );
        let c: nmbrs_metrics::controls::Control<u32> =
            nmbrs_metrics::controls::ControlBuilder::new("concurrency", 4u32)
                .reify_as_gauge(|v| Some(*v as f64))
                .from_f64(|v| {
                    if v < 0.0 || v > u32::MAX as f64 {
                        Err(format!("out of range: {v}"))
                    } else {
                        Ok(v as u32)
                    }
                })
                .branch_scope(nmbrs_metrics::controls::BranchScope::Subtree)
                .build();
        root.read().unwrap().controls().declare(c.clone());
        set_session_root(root);

        // Issue the write through the macro-authored node, built via the same
        // factory route the compiler uses (under a binding scope).
        let _scope = polydat::dsl::factory::compile_ctx::scoped_binding("feedback_loop");
        let consts = [polydat::dsl::factory::ConstArg::Str("concurrency".into())];
        let node = polydat::dsl::factory::build_node("control_set", &[], &[], &consts)
            .expect("control_set should build");
        let mut out = [Value::None];
        node.eval(&[Value::F64(64.0)], &mut out);
        assert_eq!(out[0].as_u64(), 1, "write should report submitted");

        // The write is async; yield a few times for the spawned
        // task to run through validate → fanout → commit.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if c.value() == 64u32 {
                break;
            }
        }
        assert_eq!(c.value(), 64u32);
        let committed = c.get();
        assert!(matches!(
            committed.origin,
            nmbrs_metrics::controls::ControlOrigin::Polydat { .. }
        ));

        // Idempotent-write elision: the control now reads 64.0, so a
        // second write of the SAME value is a fixpoint — elided, 0
        // (not dispatched), revision unmoved. A different value
        // dispatches again.
        let rev_before = c.get().rev;
        node.eval(&[Value::F64(64.0)], &mut out);
        assert_eq!(
            out[0].as_u64(),
            0,
            "write of the committed value must be elided"
        );
        assert_eq!(
            c.get().rev,
            rev_before,
            "an elided write must not touch the control"
        );
        node.eval(&[Value::F64(32.0)], &mut out);
        assert_eq!(out[0].as_u64(), 1, "a real change dispatches");
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if c.value() == 32u32 {
                break;
            }
        }
        assert_eq!(c.value(), 32u32);
    }

    #[test]
    fn control_u64_casts_gauge_to_integer() {
        let _g = serial_test();
        install_session_with_control("concurrency", 64);
        // `control_u64` is macro-authored — compile + pull it end to end.
        let mut k = polydat::dsl::compile_polydat("x := control_u64(\"concurrency\")")
            .expect("compile control_u64");
        assert_eq!(k.pull("x").as_u64(), 64);
    }

    #[test]
    fn control_u64_missing_name_returns_zero() {
        let _g = serial_test();
        install_session_with_control("concurrency", 5);
        let mut k = polydat::dsl::compile_polydat("x := control_u64(\"not_there\")")
            .expect("compile control_u64");
        assert_eq!(k.pull("x").as_u64(), 0);
    }

    #[test]
    fn control_u64_is_volatile_not_const_folded() {
        let _g = serial_test();
        install_session_with_control("concurrency", 32);
        // Intrinsic volatility (the bug this fixes): a `Nondeterministic`
        // node is never const-folded, so the wire stays a live dynamic
        // output re-read on every pull. A Pure reader would fold `x` to a
        // compile-time constant — which is exactly how the old hand-written
        // node (no purity override) cached a stale first value.
        let k = polydat::dsl::compile_polydat("x := control_u64(\"concurrency\")")
            .expect("compile control_u64");
        assert!(
            k.get_constant("x").is_none(),
            "control_u64 must be volatile — its wire must NOT be const-folded",
        );
    }

    #[test]
    fn control_bool_projects_gauge_to_boolean() {
        let _g = serial_test();
        install_session_with_control("enabled", 1);
        let mut k =
            polydat::dsl::compile_polydat("x := control_bool(\"enabled\")").expect("compile");
        assert!(k.pull("x").as_bool());
    }

    #[test]
    fn control_bool_zero_is_false() {
        let _g = serial_test();
        install_session_with_control("enabled", 0);
        let mut k =
            polydat::dsl::compile_polydat("x := control_bool(\"enabled\")").expect("compile");
        assert!(!k.pull("x").as_bool());
    }

    #[test]
    fn control_bool_missing_name_is_false() {
        let _g = serial_test();
        install_session_with_control("enabled", 1);
        let mut k =
            polydat::dsl::compile_polydat("x := control_bool(\"absent\")").expect("compile");
        assert!(!k.pull("x").as_bool());
    }

    #[test]
    fn control_str_renders_value_string() {
        let _g = serial_test();
        install_session_with_control("concurrency", 42);
        let mut k =
            polydat::dsl::compile_polydat("x := control_str(\"concurrency\")").expect("compile");
        // u32's Debug rendering is its decimal representation.
        assert_eq!(k.pull("x").as_str(), "42");
    }

    #[test]
    fn control_str_missing_name_returns_empty() {
        let _g = serial_test();
        install_session_with_control("concurrency", 42);
        let mut k =
            polydat::dsl::compile_polydat("x := control_str(\"log_level\")").expect("compile");
        assert_eq!(k.pull("x").as_str(), "");
    }

    #[tokio::test]
    async fn control_set_records_compile_time_binding_attribution() {
        let _g = serial_test();
        // Build a control that accepts f64 writes and install
        // the session root.
        let root = Component::root(
            Labels::empty().with("session", "attr"),
            std::collections::HashMap::new(),
        );
        let c: nmbrs_metrics::controls::Control<f64> =
            nmbrs_metrics::controls::ControlBuilder::new("rate", 100.0)
                .reify_as_gauge(|v| Some(*v))
                .from_f64(Ok)
                .branch_scope(nmbrs_metrics::controls::BranchScope::Subtree)
                .build();
        root.read().unwrap().controls().declare(c.clone());
        set_session_root(root.clone());

        // Simulate a compiler constructing a control_set factory
        // under a binding scope named `rate_adj`. We can't call
        // `build_node` from inside the same crate's private
        // factory path directly, so reach into the nodes
        // registration helper via the same build-by-name route
        // the compiler uses.
        let _scope = polydat::dsl::factory::compile_ctx::scoped_binding("rate_adj");
        let consts = [polydat::dsl::factory::ConstArg::Str("rate".into())];
        let node = polydat::dsl::factory::build_node("control_set", &[], &[], &consts)
            .expect("control_set should build");
        let mut out = [Value::None];
        node.eval(&[Value::F64(4242.0)], &mut out);

        // Let the spawned write complete.
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if c.value() == 4242.0 {
                break;
            }
        }
        assert_eq!(c.value(), 4242.0);
        match c.get().origin {
            nmbrs_metrics::controls::ControlOrigin::Polydat { ref binding } => {
                assert_eq!(
                    binding, "rate_adj",
                    "attribution should be the DSL binding name, not the control name"
                );
            }
            other => panic!("expected Polydat origin, got {other:?}"),
        }
    }

    #[test]
    fn control_set_returns_zero_without_session_root() {
        let _g = serial_test();
        // Explicitly clear the session root so the node can't
        // resolve anything.
        *SESSION_ROOT.lock().unwrap_or_else(|e| e.into_inner()) = None;

        let consts = [polydat::dsl::factory::ConstArg::Str("anything".into())];
        let node = polydat::dsl::factory::build_node("control_set", &[], &[], &consts)
            .expect("control_set should build");
        let mut out = [Value::None];
        node.eval(&[Value::F64(1.0)], &mut out);
        assert_eq!(out[0].as_u64(), 0);
    }

    // ---- SRD-89: per-execution control isolation -------------------

    /// Build a one-entry control map carrying a `concurrency` control fixed at
    /// `val`, as `install_controls` expects (name → erased handle).
    /// A standalone phase-tier component declaring a `concurrency` control at
    /// `val` — the per-execution phase component a fiber resolves against.
    fn component_with_concurrency(val: u32) -> Arc<RwLock<Component>> {
        let comp = Component::root(Labels::empty().with("phase", "p"), HashMap::new());
        comp.read()
            .unwrap_or_else(|e| e.into_inner())
            .controls()
            .declare(
                ControlBuilder::new("concurrency", val)
                    .reify_as_gauge(|v| Some(*v as f64))
                    .branch_scope(BranchScope::Subtree)
                    .build(),
            );
        comp
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn per_execution_control_map_isolates_concurrent_reads() {
        // SRD-89 §2b — two executions sharing one session each declare a
        // `concurrency` control with a DISTINCT value on their own phase
        // component. Under the pre-SRD-89 shared-`SESSION_ROOT` model both
        // resolve to one instance (deterministic cross-talk — a servo
        // retargeting one drives the other). The per-execution control map
        // isolates them: each execution reads its OWN setting. This is the unit
        // encoding of the walker's `control`/`multiservo` failure.
        let _g = serial_test();
        // Clear the global root so resolution can ONLY come from each fiber's
        // own current-component snapshot (a leftover sibling root must not mask
        // a miss).
        *SESSION_ROOT.lock().unwrap_or_else(|e| e.into_inner()) = None;

        // Two executions, each with its OWN phase component declaring
        // `concurrency` at a distinct value.
        let comp_a = component_with_concurrency(2);
        let comp_b = component_with_concurrency(32);
        let phase: Arc<str> = Arc::from("p");

        // Each fiber resolves through its own component snapshot (FIBER_CTX).
        let a_val = with_fiber_context(phase.clone(), snapshot_controls(&comp_a), async {
            control_gauge_f64("concurrency")
        })
        .await;
        let b_val = with_fiber_context(phase.clone(), snapshot_controls(&comp_b), async {
            control_gauge_f64("concurrency")
        })
        .await;

        assert_eq!(a_val, 2.0, "execution A must read its OWN concurrency (2)");
        assert_eq!(
            b_val, 32.0,
            "execution B must read its OWN concurrency (32), not A's",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn control_read_falls_back_to_session_root_without_exec_context() {
        // A1 — outside any scoped execution (single-run / dryrun / TUI), the
        // read path is byte-identical to before SRD-89: it resolves via the
        // global SESSION_ROOT walk. No per-exec map exists, so the fallback is
        // the only path.
        let _g = serial_test();
        install_session_with_control("concurrency", 7);
        // No execution_context::scope here — current_control() returns None.
        assert_eq!(control_gauge_f64("concurrency"), 7.0);
    }
}
