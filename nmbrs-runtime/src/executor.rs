// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Recursive scenario tree executor.
//!
//! Walks `ScenarioNode` trees dynamically at runtime. All control
//! flow constructs (`for_each`, `do_while`, `do_until`) are evaluated
//! uniformly — no pre-flattening. Polydat scope composition handles
//! variable scoping at every nesting level.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::activity::{Activity, ActivityConfig};
use crate::adapter::DriverAdapter;
use crate::opseq::{OpSequence, SequencerType};
use crate::synthesis::OpBuilder;
use nmbrs_metrics::cadence_reporter::CadenceReporter;
use nmbrs_metrics::component::{self, Component, ComponentState};
use nmbrs_metrics::labels::Labels;
use nmbrs_workload::model::{ScenarioNode, WorkloadPhase};
use polydat::kernel::{ScopeCoord, format_scope_coordinate_path};

/// SRD-83 follow-up — resolve a stop condition's action scope from its
/// explicit `at:` target and its `per:`/`each:` detection levels. `at`
/// wins; otherwise the innermost (most specific) detection level is used,
/// so a rule with no `at:` acts exactly where it is detected (the
/// historical behaviour, preserved for backward compatibility).
pub(crate) fn resolve_stop_scope(
    at: Option<nmbrs_workload::model::ScopeLevel>,
    each: &[nmbrs_workload::model::ScopeLevel],
) -> crate::stop_conditions::StopScope {
    use crate::stop_conditions::StopScope as S;
    use nmbrs_workload::model::ScopeLevel as L;
    fn rank(l: L) -> u8 {
        match l {
            L::SelfScope => 0,
            L::Op => 1,
            L::Phase => 2,
            L::Scenario => 3,
            L::Workload => 4,
        }
    }
    let level = at.or_else(|| each.iter().copied().min_by_key(|l| rank(*l)));
    match level {
        Some(L::Scenario) => S::Scenario,
        Some(L::Workload) => S::Workload,
        _ => S::Phase,
    }
}

/// Shared context for the recursive executor.
///
/// `Clone` is derived so the concurrent scheduler can fork per-task
/// copies: every Arc field aliases cheaply, while the mutable
/// `label_stack` forks so each concurrent sibling carries its own
/// label path.
#[derive(Clone)]
pub struct ExecCtx {
    pub phases: HashMap<String, WorkloadPhase>,
    /// SRD-71 P3 — phase-scoped CLI parameter overrides
    /// (`<phase-pattern>.<param>=<value>`). Resolved per phase
    /// at scope activation (exact name beats glob; ambiguous
    /// globs are fatal) and written onto the phase's kernel
    /// locally so the standard scope chain serves the
    /// overridden value to everything below.
    pub phase_param_overrides: Arc<Vec<crate::phase_params::PhaseParamOverride>>,
    /// Workload-level `readouts:` bindings (SRD-63 §5).
    /// Threaded through ActivityConfig at phase construction
    /// so each activity-init step builds a binder seeded
    /// with the configured slot bindings on top of the
    /// built-in defaults.
    pub workload_readouts: nmbrs_workload::model::ReadoutsBindings,
    /// CLI `--readout=<body>` override (SRD-63 §8 / Push 8).
    /// When `Some`, replaces the workload's `on_update`
    /// binding for the duration of this run. Either a
    /// known readout name (`phase_status`, `trace`, etc.)
    /// or a literal body string (parsed by the body
    /// grammar). `None` falls through to the workload
    /// binding.
    pub cli_readout_override: Option<String>,
    pub workload_params: HashMap<String, String>,
    /// SRD-32a Push 3 — workload-root wrapper-composition
    /// override. Innermost-to-outermost list, threaded
    /// through to each phase's Activity construction.
    /// `None` ⇒ activities use the resolver's built-in
    /// default-order tiebreaker.
    pub wrappers_override: Option<Vec<String>>,
    /// SRD-32a Push 3 — CLI `--wrap-default-order` override.
    /// Innermost-to-outermost list that REPLACES the
    /// resolver's built-in `DEFAULT_ORDER` tiebreaker for
    /// this run. Validated against the constraint graph at
    /// resolver construction; a malformed list aborts the
    /// session at activity start. Distinct from
    /// `wrappers_override`: that pins the per-op stack
    /// directly (must be a permutation of triggered
    /// wrappers); this only changes the tiebreaker the
    /// resolver uses when constraints leave order ambiguous.
    pub wrap_default_order: Option<Vec<String>>,
    pub program: Arc<polydat::kernel::PolydatProgram>,
    pub polydat_lib_paths: Vec<PathBuf>,
    pub workload_dir: Option<PathBuf>,
    pub strict: bool,
    pub driver: String,
    pub merged_params: HashMap<String, String>,
    pub dry_run: Option<&'static str>,
    /// Compiled `phases=<pattern>` filter. When `Some`, the
    /// scenario walker skips phase activations whose name does
    /// not match, and elides any scope subtree whose descendant
    /// phases all fail to match. When `None`, every phase runs.
    /// See [`crate::phase_filter::PhasePattern`] for the dialect
    /// rules (bareword / glob / regex).
    pub phase_filter: Option<Arc<crate::phase_filter::PhasePattern>>,
    /// SRD-77 refine plan. When `Some`, the scenario walker
    /// skips phase activations whose `(name, phase_labels)` is
    /// already in the plan's `completed` set (a prior execution
    /// in this session finished them). When `None`, every
    /// non-`phase_filter`-elided phase runs. Composes additively
    /// with `phase_filter`: a phase is dispatched iff both
    /// filters allow it.
    pub refine_plan: Option<Arc<crate::refine_plan::RefinePlan>>,
    pub diag: crate::runner::DiagnosticConfig,
    /// True during the runner's pre-map structural pass. Pre-map
    /// walks the scenario tree at depth=Phase to populate the
    /// global `SceneTree` for the TUI/summary observers, but it
    /// is NOT execution — no phase has actually run, no dispenser
    /// has been built, no op has fired. "Completion" is an
    /// undefined concern at that point.
    ///
    /// The structural walker's `depth < Dispenser` branch fires
    /// sentinel `set_phase_running` + `set_phase_completed`
    /// status mutations to make the `dryrun=phase` post-run
    /// summary show `[ok]` for every traversed phase. When
    /// `pre_map_only == true` those mutations are suppressed —
    /// the scene tree comes out with every phase still
    /// `Pending`, which is what the TUI margin reads when it
    /// computes `seq` / `done / total`.
    ///
    /// `dryrun=phase` and the real execution paths both leave
    /// this `false`. Only the pre-map pass sets it.
    pub pre_map_only: bool,
    pub seq_type: SequencerType,
    pub concurrency: usize,
    pub rate: Option<f64>,
    pub error_spec: String,
    /// Workload-root total-attempts budget (the `tries` param). Each phase
    /// resolves its effective budget as its own `tries:` over this; ops may
    /// override with their own `tries:` field. `None` = no budget anywhere →
    /// the conditional tries wrapper is not constructed (single attempt,
    /// SRD-82 Part 3b). `Some(0)` = ops fail without executing; `Some(1)` =
    /// explicit single-attempt.
    pub tries: Option<u32>,
    /// Session-wide error-rate circuit-breaker default (Feature B).
    /// Each phase resolves its effective threshold as its own
    /// `error_rate_max:` over this. `None` = disabled by default.
    pub error_rate_max: Option<f64>,
    /// SRD-82 — the current (inherited) [`crate::error_policy::ErrorPolicy`]
    /// at this point in the walk. Seeded with the session root policy
    /// (from `error_spec` + `error_rate_max`); each phase shell resolves
    /// its own from this via `resolve_child`, inheriting or deriving a
    /// value-equality-shared instance.
    pub error_policy: Arc<crate::error_policy::ErrorPolicy>,
    /// Session identifier for metric labeling. Surfaces as
    /// the `session` dimensional label on every per-component
    /// metric via [`Self::labels`].
    pub session_id: String,
    /// SRD-77 — active execution id within the session.
    /// Defaults to `1` until SRD-77's `refine` verb lands the
    /// per-session registry that bumps it. Surfaces as the
    /// `exec_id` dimensional label.
    pub exec_id: u64,
    /// Workload's bare stem (filename without path or
    /// extension; `"workload"` fallback for inline /
    /// op-only runs). Surfaces as the `workload=…` label
    /// on every metric — cross-session queries (e.g.
    /// "compare last week's full_cql_vector runs") group
    /// on this rather than on the path.
    pub workload_name: String,
    /// Label stack: accumulated dimensional labels from the component tree.
    /// for_each pushes (var, value), phase pushes ("phase", name).
    /// do_while/do_until are transparent — they don't contribute labels.
    pub label_stack: Vec<(String, String)>,
    /// Session root component (owns the component tree).
    pub session_component: Arc<RwLock<Component>>,
    /// Cadence reporter for lifecycle flush — same reporter the
    /// scheduler is feeding; end-of-phase final deltas route here.
    pub cadence_reporter: Arc<CadenceReporter>,
    /// Scheduler stop handle for delivering frames to reporters.
    pub stop_handle: Arc<nmbrs_metrics::scheduler::StopHandle>,
    /// Run observer for phase lifecycle events (TUI or stderr).
    pub observer: Arc<dyn crate::observer::RunObserver>,
    /// Canonical scope tree for the current scenario (SRD 18b).
    /// Built once per session from the resolved scenario nodes;
    /// mirrors the scenario structure 1:1 with parent / child
    /// pointers, depth tags, and pragma slots. Today consumed by
    /// observer pre-mapping and diagnostic display; future steps
    /// (extern-binding migration, scheduler) drive execution off
    /// this tree directly.
    pub scope_tree: Arc<crate::scope_tree::ScopeTree>,
    /// Per-level concurrency policy (SRD 18b §"Scheduler
    /// abstraction"). Consulted by the tree walker at each depth
    /// to decide whether sibling scopes / for_each iterations run
    /// serially or concurrently. Shared across forked clones —
    /// the spec is immutable after session construction.
    pub schedule_spec: Arc<crate::scheduler::ScheduleSpec>,
    /// M3.4 — current immediate-parent scope kernel for the
    /// leaf phase compile. Set by the dependent-tuple
    /// dispatcher to the per-branch `PolydatKernel` it owns; cleared
    /// (or restored) when the dispatcher unwinds. When `Some`,
    /// the leaf-phase compile path uses this kernel's manifest
    /// for auto-extern wiring and calls `materialize_wiring_from_outer`
    /// against it directly — iteration vars and inherited
    /// values both flow through the standard Polydat chain. When
    /// `None`, the leaf phase falls back to the workload-level
    /// `outer_manifest` / `outer_scope_values` (the legacy flat
    /// data flow that M3.4 retires for kernel-routed scopes).
    pub current_parent_kernel: Option<Arc<polydat::kernel::PolydatKernel>>,
    /// Workload source text + path, kept for error diagnostics.
    /// Errors at the dispatch layer (for_each / do_while spec
    /// evaluation, interpolation failures) include the YAML
    /// line / column where the failing spec was authored, so
    /// the user can jump straight to the source. `None` for
    /// inline workloads (`op=`) where there's no file.
    pub workload_source: Option<Arc<WorkloadSource>>,
    /// Per-session checkpoint writer (SRD-44). `None` when the
    /// session was constructed without checkpointing (e.g.
    /// short test fixtures). When present, the executor calls
    /// `declare_phase` during pre-map and `phase_started` /
    /// `phase_completed` / `phase_failed` around the dispatch
    /// of every leaf phase.
    pub checkpoint_writer: Option<Arc<crate::checkpoint::CheckpointWriter>>,
    /// Resume plan derived from a saved checkpoint document.
    /// Defaults to `ResumePlan::fresh()` (every phase re-runs)
    /// when no checkpoint was loaded; an `Arc` so concurrent
    /// scheduler forks share the same plan.
    pub resume_plan: Arc<crate::checkpoint::ResumePlan>,
    /// SQLite metrics reporter handle, threaded through here so
    /// the executor can purge prior-invocation sample rows on
    /// resume re-runs (SRD-44 §"Wholesale metrics-purge"). The
    /// `Arc<Mutex<Option<...>>>` shape mirrors the runner-side
    /// declaration: `None` when SQLite is disabled (in-memory
    /// adapters, fixture tests).
    pub sqlite_reporter:
        Arc<std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>>,
    /// Driver-resource sharing pool (SRD-35). Owns the
    /// lifecycle of long-lived shared resources across
    /// phases — adapter shells attach via
    /// [`crate::resource_pool::attach`] for the duration
    /// of one phase activation. Push A: every adapter goes
    /// through the pool under PerPhase semantics (one
    /// resource per phase, byte-identical to today) but
    /// the lifecycle event quartet (`resource.attach` /
    /// `resource.init.*` / `resource.detach` /
    /// `resource.close.*`) lands at every boundary.
    pub resource_pool: Arc<crate::resource_pool::ResourcePool>,
    /// Current SceneTree parent node id — the walker pushes
    /// child nodes under this id as it descends each
    /// scenario-tree arm. Saved+restored on the way out of
    /// each arm, so siblings share the same parent and
    /// children land under their own scope. Per SRD 18b
    /// §"Single Walker Contract" point 2 + §"Display: flat
    /// or hierarchical, same source": the walker IS what
    /// builds the SceneTree, not a separate pre-pass.
    pub scene_tree_parent_id: crate::scene_tree::SceneNodeId,
    /// Current SceneTree YAML path — the walker appends one
    /// `PathSegment` per arm (Scenario / Phase / ForEach /
    /// ForCombinations / DoWhile / DoUntil / ScenarioInclude
    /// / bindings) and assigns it to each created node via
    /// `set_yaml_path`. Per SRD-44 §"Phase identity".
    pub scene_tree_path: Vec<crate::checkpoint::PathSegment>,
    /// Current scope-tree position threaded through the
    /// scenario-tree walker.
    ///
    /// **TRANSITIONAL WORKAROUND (task #19):** the AST alone
    /// is not yet self-identifying — two scope-tree positions
    /// can share AST/source but produce semantically-distinct
    /// installed kernels via different parent-chain cascades.
    /// Carrying the current position through the walker lets
    /// `find_*_scope_under(parent_idx, ast)` add the missing
    /// context at the lookup site. When the AST becomes the
    /// canonical sole-source identity, this field becomes
    /// unnecessary and can be removed along with the `_under`
    /// variants and their save/restore plumbing in the
    /// Comprehension and Bindings arms.
    ///
    /// Defaults to 0 (the workload root). Each Comprehension /
    /// Bindings descent pushes the matched scope_idx; restored
    /// on the way out.
    pub current_scope_idx: crate::scope_tree::ScopeNodeIdx,
    /// SRD-83 — the workload execution shell (SRD-82's outermost
    /// shell). Holds the live child-phase aggregate (`children_*`,
    /// `op_count`, `error_count`) and the workload's compiled stop
    /// conditions. Shared (`Arc`) across cloned task contexts so a
    /// phase finishing anywhere in the scenario tree feeds the same
    /// accumulator; `run_phase` calls `record_phase` per outcome and
    /// the walker consults `should_stop` before each sibling. See
    /// [`crate::workload_shell::WorkloadShell`].
    pub workload_shell: Arc<crate::workload_shell::WorkloadShell>,
    /// SRD-83 — the workload-level `stop_when:` declarations, kept so
    /// the per-phase activity build can gather the ones whose `each:`
    /// names `phase` (distributing a workload declaration down to each
    /// phase shell) alongside the phase's own predicates. The
    /// `each ∋ {self, workload}` subset drives `workload_shell` and is
    /// already compiled into it; this is the unfiltered source for the
    /// structural `each:` fan-out at the phase level.
    pub workload_stop_when: Vec<nmbrs_workload::model::StopConditionSpec>,
    /// SRD-82 Part 6 — when this context is dispatching a DAEMON phase,
    /// the scenario shell sets this to the daemon-group's completion flag
    /// (latched once the scope's foreground phases finish). `run_phase`
    /// threads it onto the daemon phase's `Activity::daemon_stop` so its
    /// fibers stop cooperatively. `None` for foreground phases.
    pub daemon_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// SRD-86 — when set (by `dispatch_optimization` during an optimize-node
    /// search), `run_phase` pulls this objective wire off the phase's **single
    /// live kernel** just before execution and stores the value in
    /// `optimize_objective_value`. `None` everywhere else. The objective must be
    /// a wire fully qualified on the phase node (validity = ordinary kernel
    /// compilation); there is no second/reconstructed kernel.
    pub optimize_objective: Option<String>,
    /// The value `run_phase` pulled for `optimize_objective` this iteration.
    pub optimize_objective_value: Option<f64>,
    /// SRD-86 Control-class actuation — when set (by `dispatch_optimization`'s
    /// Control branch), `run_phase` runs ONE continuous phase and `tokio::join!`s
    /// the [`servo`](crate::optimize::servo::servo) daemon alongside the activity
    /// loop: the daemon live-retargets the phase's controls per setting instead
    /// of rerunning. `None` for the Coordinate path (and `optimize_objective`
    /// stays `None` here — the servo owns settling, not `run_phase`).
    pub optimize_servo: Option<crate::optimize::servo::ServoSpec>,
}

/// Workload YAML source kept alongside the parsed model so
/// runtime errors can report YAML line/column locations.
pub struct WorkloadSource {
    pub path: String,
    pub text: String,
}

impl WorkloadSource {
    /// Find the first occurrence of `needle` in the source
    /// text and return its 1-indexed (line, column). Returns
    /// `None` if `needle` doesn't appear (e.g. it was
    /// dynamically constructed and isn't a substring of the
    /// authored YAML).
    pub fn locate(&self, needle: &str) -> Option<(usize, usize)> {
        let idx = self.text.find(needle)?;
        let prefix = &self.text[..idx];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let col = prefix.rfind('\n').map(|nl| idx - nl).unwrap_or(idx + 1);
        Some((line, col))
    }
}

/// Format an error with a YAML-source location prefix when one
/// can be found. Used at dispatch sites (for_each, do_while)
/// to enrich downstream error messages with `<path>:<line>:<col>`.
///
/// Idempotent under nesting: if the error already starts with
/// this workload's location prefix (because an inner dispatcher
/// already enriched it), the outer wrapper leaves it alone — no
/// double-prefix.
pub(crate) fn enrich_with_yaml_location(ctx: &ExecCtx, needle: &str, err: String) -> String {
    let Some(src) = ctx.workload_source.as_ref() else {
        return err;
    };
    if err.starts_with(&format!("{}:", src.path)) {
        return err;
    }
    let Some((line, col)) = src.locate(needle) else {
        return err;
    };
    format!("{}:{line}:{col}: {err}", src.path)
}

/// SRD-92 — yaml-enrich a child subtree's [`crate::phase_outcome::Outcome`]
/// in place: if it failed, rewrite its `reason` with the source location
/// (the Outcome analog of [`enrich_with_yaml_location`], preserving the
/// two-axis disposition instead of collapsing to a `Result`).
fn enrich_outcome(
    ctx: &ExecCtx,
    needle: &str,
    mut o: crate::phase_outcome::Outcome,
) -> crate::phase_outcome::Outcome {
    if o.is_failure() {
        let m = enrich_with_yaml_location(ctx, needle, o.reason.take().unwrap_or_default());
        o = o.with_reason(m);
    }
    o
}

impl ExecCtx {
    /// Build Labels from the current label stack.
    ///
    /// Always seeded with `session` (the run's id) and
    /// `workload` (the workload file's bare stem) so every
    /// metric inherits both. The `workload` label is a
    /// stable name that's invariant across `path/` shifts
    /// and `--session-name` overrides — it's what
    /// cross-session queries should group on.
    pub fn labels(&self) -> Labels {
        let mut labels = Labels::of("session", &self.session_id)
            .with("exec_id", self.exec_id.to_string())
            .with("workload", &self.workload_name);
        for (k, v) in &self.label_stack {
            labels = labels.with(k, v);
        }
        labels
    }

    /// The labels OWNED at this scope depth — the live label
    /// stack only (`for_each` iteration labels + `phase`), WITHOUT
    /// the `{session, exec_id, workload}` prefix that
    /// [`Self::labels`] seeds.
    ///
    /// Use this for a component's *own* labels: `session` is owned
    /// by the session component and `{exec_id, workload}` by the
    /// execution component (SRD-88 §2), so a phase/activity
    /// component below them must NOT redeclare those names — the
    /// label-ownership invariant is that each name is set on
    /// exactly one tier and inherited downward. The full effective
    /// set is recomposed by [`component::attach`] from the
    /// ancestor chain; [`Self::labels`] remains the right call
    /// when a caller needs that full set directly (e.g. matching
    /// metric instances for a resume purge).
    pub fn incremental_labels(&self) -> Labels {
        let mut labels = Labels::empty();
        for (k, v) in &self.label_stack {
            labels = labels.with(k, v);
        }
        labels
    }

    /// Push a label onto the stack.
    pub fn push_label(&mut self, key: &str, value: &str) {
        self.label_stack.push((key.to_string(), value.to_string()));
    }

    /// Pop the top label from the stack.
    pub fn pop_label(&mut self) {
        self.label_stack.pop();
    }

    /// Whether stderr diagnostic output is suppressed (TUI handles display).
    pub fn quiet(&self) -> bool {
        self.observer.suppresses_stderr()
    }
}

/// Execute a scenario tree recursively.
///
/// Entry point — siblings are at scope-tree depth 0. The per-depth
/// concurrency policy lives on `ctx.schedule_spec`; when the policy
/// at some depth allows >1 concurrency, siblings (and ForEach /
/// phase-level-for_each iterations at the next depth) fork via
/// cloned per-task `ExecCtx`.
pub fn execute_tree<'a>(
    ctx: &'a mut ExecCtx,
    nodes: &'a [ScenarioNode],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    // The runner boundary stays `Result` (exit-code / run-result mapping in
    // runner.rs is unchanged). Project the walker's two-axis Outcome here.
    Box::pin(async move {
        let o = execute_tree_at(ctx, nodes, 0).await;
        if o.is_failure() {
            Err(o
                .reason
                .clone()
                .unwrap_or_else(|| "scenario: a unit failed".to_string()))
        } else {
            Ok(())
        }
    })
}

// ─── SRD-82 execution shells ──────────────────────────────────────
//
// The scenario graph is SRD-82's outermost execution shell. A shell
// runs its BODY (child units, via the one SRD-02 concurrency path),
// HANDLES each unit's result through its handler (the scenario policy),
// and AGGREGATES into a two-axis `Outcome`. The phase / stanza / op
// shells follow the same shape as the unification proceeds.

/// SRD-82 Part 3 — what the shell handler decides for one child result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellAction {
    /// Keep running the rest of the body.
    Continue,
    /// Halt the body now (the SRD-82 `stop` action).
    Stop,
}

/// SRD-82 Part 3 — the scenario-graph handler (the "scenario policy").
/// Today it is the `*Failed:stop` default: a child whose `Outcome` is
/// `Failed` halts the body. Generalising it to the full router
/// (child-validity / aggregate match keys, `count`/`warn`/`retry`/`fail`
/// actions) is SRD-82 Part 5 step 2.
#[derive(Debug, Clone, Copy)]
struct ShellHandler;

impl ShellHandler {
    fn scenario_default() -> Self {
        ShellHandler
    }
    /// Decide the action for one child's outcome.
    fn decide(&self, child: &crate::phase_outcome::Outcome) -> ShellAction {
        if child.is_failure() {
            ShellAction::Stop
        } else {
            ShellAction::Continue
        }
    }
}

/// SRD-92 — the tagging trait EVERY execution shell implements, at every
/// nesting level (session · scenario · phase · stanza · op). A shell is the
/// smallest thing that can be asked to run and answer with an
/// [`crate::phase_outcome::Outcome`]. Its contract today is exactly the two
/// aspects verified semantically UNIFORM across all five layers (the
/// aspect×layer alignment grid):
///   * OUTCOME — produce one two-axis `Outcome` (the sole upward signal); a
///     leaf PRODUCES it, a composite FOLDS it from children.
///   * POLL — obey one cooperative stop at every boundary; a stop yields a
///     clean `Interrupted`, never a forced kill.
///
/// Realign in next (the grid's `UNIFORM_SHAPE` tier), each behind ONE typed
/// knob: GUARD · WALK-AT-DEPTH · OPEN · FOLD · CLOSE · EMIT. Capabilities
/// and level-intrinsic state (the child-set generator, the concurrency
/// category, the lateral results carrier, daemon, rate, all world/state)
/// stay OUT — they are not trait members.
// `run` is LIVE (the phase/scenario shells + the generic wrapper layers drive
// it). `poll_stop` / `shell_kind` are the contract's other two aspects, still
// awaiting their consumers as the unification lands — hence the allow.
#[allow(dead_code)]
pub(crate) trait ExecShell: Send + Sync {
    /// OUTCOME. Run this shell to a terminal `Outcome`. The shell value
    /// carries whatever its body needs (a composite holds its node-set; a
    /// phase its name; an op its dispenser), so the universal signature
    /// takes NO node list — a leaf has none.
    fn run<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
    >;

    /// POLL. The one cooperative stop, consulted at every boundary. The
    /// default folds the existing per-execution stop signals (a fault
    /// outranks a graceful / walk stop); the Step-0 `StopView` consolidation
    /// will replace the body WITHOUT changing this signature.
    fn poll_stop(&self, ctx: &ExecCtx) -> Option<crate::session_signals::StopCause> {
        use crate::session_signals::{self as sig, StopCause};
        if sig::fault_stop_requested() {
            Some(StopCause::Fault)
        } else if sig::stop_requested() || ctx.workload_shell.should_stop() {
            Some(StopCause::Interrupt)
        } else {
            None
        }
    }

    /// Stable identity for projection / introspection (EMIT, dryrun) and the
    /// per-level knob lookups the uniform-shape aspects will key on.
    fn shell_kind(&self) -> ShellKind;
}

/// SRD-92 — which level a shell sits at (for EMIT / dryrun / per-level knobs).
#[allow(dead_code)] // WIP SRD-92 — Stanza/Session variants land with their shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellKind {
    Session,
    Scenario,
    Phase,
    Stanza,
    Op,
}

/// SRD-92 — a shell whose BODY is a child node-set, dispatched through the
/// one concurrency path ([`run_scenario_body`]). The Composite half of
/// Composite-vs-Leaf: the scenario graph today, the workload tier next.
/// Leaf shells (phase / stanza / op) do NOT implement this — they PRODUCE
/// an `Outcome`, they do not dispatch a node list (SRD-92 Decision B).
#[allow(dead_code)] // WIP SRD-92.
trait CompositeShell: ExecShell {
    fn handler(&self) -> &ShellHandler;

    /// BODY + HANDLE + AGGREGATE over the node-set. Returns the aggregate
    /// `Outcome` plus the first-failure reason (carried alongside until the
    /// later `Outcome.reason` enrichment), which the bare two-axis `Outcome`
    /// cannot hold.
    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
        nodes: &'a [ScenarioNode],
        depth: usize,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
    >;
}

/// The scenario-graph shell (SRD-82/92) — a [`CompositeShell`]. Carries the
/// scenario handler plus the node-set + depth it was built for, so it also
/// satisfies the universal [`ExecShell`] (`run` -> `Outcome`).
struct ScenarioShell<'n> {
    handler: ShellHandler,
    nodes: &'n [ScenarioNode],
    depth: usize,
}

impl<'n> ScenarioShell<'n> {
    fn scenario(nodes: &'n [ScenarioNode], depth: usize) -> Self {
        Self {
            handler: ShellHandler::scenario_default(),
            nodes,
            depth,
        }
    }
}

impl<'n> ExecShell for ScenarioShell<'n> {
    fn run<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
    > {
        Box::pin(async move { self.dispatch(ctx, self.nodes, self.depth).await })
    }
    fn shell_kind(&self) -> ShellKind {
        ShellKind::Scenario
    }
}

impl<'n> CompositeShell for ScenarioShell<'n> {
    fn handler(&self) -> &ShellHandler {
        &self.handler
    }
    fn dispatch<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
        nodes: &'a [ScenarioNode],
        depth: usize,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
    > {
        Box::pin(run_scenario_body(&self.handler, ctx, nodes, depth))
    }
}

/// Fold a joined child task into its [`crate::phase_outcome::Outcome`] +
/// reason. SRD-92: `execute_node` now produces the child's REAL two-axis
/// Outcome, so this no longer re-projects a `Result` — it passes the
/// child's Outcome through (surfacing its own `reason` for the aggregate
/// fold) and only maps the one remaining lossy case, a task **panic**
/// (which has no Outcome), to a fault.
fn join_outcome(
    res: Result<crate::phase_outcome::Outcome, tokio::task::JoinError>,
) -> (crate::phase_outcome::Outcome, Option<String>) {
    use crate::phase_outcome::Outcome;
    match res {
        // A panicked task: the only place a lossy projection remains — a
        // panic has no Outcome, so it becomes a fault.
        Err(join_err) => {
            let msg = format!("concurrent task panicked: {join_err}");
            (Outcome::failed().with_reason(msg.clone()), Some(msg))
        }
        // SRD-92 flow-up: the child task now carries its REAL two-axis
        // Outcome (produced by `run_phase` / the scenario subtree) — no
        // `Result -> Outcome` re-projection. Surface its own reason for the
        // aggregate fold.
        Ok(outcome) => {
            let reason = outcome.reason.clone();
            (outcome, reason)
        }
    }
}

/// SRD-92 two-latch fold step. Updates **two independent latches** from one
/// joined child:
/// - `any_failed_reason` — the validity latch: set by ANY failed child
///   (`is_failure`), *independent* of the stop decision, so a non-stop policy
///   that lets a failed child through still surfaces as `Completed+Failed`
///   rather than a lost failure.
/// - `first_failure` — the cascade-stop trigger + reason: set only when the
///   handler decides to `Stop` (drives the dispatch break + the `Interrupted`
///   disposition), exactly as before.
/// The aggregate reads the two latches separately when building its `Outcome`.
fn fold_child(
    res: Result<crate::phase_outcome::Outcome, tokio::task::JoinError>,
    handler: &ShellHandler,
    first_failure: &mut Option<String>,
    any_failed_reason: &mut Option<String>,
) {
    let (child, reason) = join_outcome(res);
    if child.is_failure() && any_failed_reason.is_none() {
        *any_failed_reason = reason.clone();
    }
    if matches!(handler.decide(&child), ShellAction::Stop) && first_failure.is_none() {
        *first_failure = reason;
    }
}

/// SRD-92 two-latch aggregate fold → the shell's two-axis `Outcome`.
/// `disposition` and `validity` are INDEPENDENT axes:
/// - `validity` = `Failed` iff a child failed — via the cascade-stop reason
///   (`first_failure`) OR the validity latch (`any_failed_reason`), so a
///   non-stop policy that let a failed child through still surfaces (the
///   `Completed+Failed` quadrant), rather than a lost failure.
/// - `disposition` = `Interrupted` iff dispatch was CUT SHORT (a handler
///   cascade-stop or a workload-shell halt), else `Completed`.
/// Quadrants: stop-policy fail → `Interrupted+Failed`; non-stop fail (all ran)
/// → `Completed+Failed`; workload halt → `Interrupted+Succeeded`; clean →
/// `Completed+Succeeded`. (`to_status()` is `Failed` for either failed quadrant.)
fn fold_aggregate(
    first_failure: Option<String>,
    any_failed_reason: Option<String>,
    should_stop: bool,
) -> crate::phase_outcome::Outcome {
    use crate::phase_outcome::{Disposition, Outcome, Validity};
    let cut_short = first_failure.is_some() || should_stop;
    let disposition = if cut_short {
        Disposition::Interrupted
    } else {
        Disposition::Completed
    };
    let validity = if first_failure.is_some() || any_failed_reason.is_some() {
        Validity::Failed
    } else {
        Validity::Succeeded
    };
    let mut outcome = Outcome::new(disposition, validity);
    if let Some(reason) = first_failure.or(any_failed_reason) {
        outcome = outcome.with_reason(reason);
    }
    outcome
}

/// SRD-92 — the PHASE leaf shell: the walker's leaf, an [`ExecShell`] (the
/// universal marker, NOT [`CompositeShell`] — a phase has no node list,
/// Decision B). Carries the phase name; `run` drives the phase's `Activity`
/// and returns its two-axis [`crate::phase_outcome::Outcome`].
///
/// WIP: today `run` projects `run_phase`'s `Result` (the faithful proxy,
/// like [`join_outcome`]). The "flow real Outcome up" step makes
/// `run_phase` return the `Outcome` it already computes internally — but
/// that first needs `Outcome` to carry a `reason` (the `?` path at the
/// phase call sites propagates the failure message, which the bare two-axis
/// `Outcome` cannot). Once enriched, `execute_node` calls this directly and
/// `join_outcome` goes away.
struct PhaseShell<'p> {
    name: &'p str,
    /// The phase's dispatch-time scene-tree row key. Carried explicitly
    /// (not read off `ctx.scene_tree_parent_id`) because the two dispatch
    /// sites differ: `execute_node` pushes a FRESH child node for the phase
    /// (distinct from its parent), while the comprehension/optimize path
    /// pre-sets `scene_tree_parent_id` to the per-iter phase node itself.
    /// The shell records whichever its caller resolved.
    node_id: crate::scene_tree::SceneNodeId,
}

impl<'p> PhaseShell<'p> {
    fn new(name: &'p str, node_id: crate::scene_tree::SceneNodeId) -> Self {
        Self { name, node_id }
    }
}

impl<'p> ExecShell for PhaseShell<'p> {
    fn run<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
    > {
        // SRD-92 — run_phase returns the two-axis Outcome directly; the
        // PhaseShell leaf forwards it (no Result re-derive). This is the
        // per-phase seam the `WrapperLevel::Phase` layers wrap.
        Box::pin(async move { run_phase(ctx, self.name, self.node_id).await })
    }
    fn shell_kind(&self) -> ShellKind {
        ShellKind::Phase
    }
}

/// SRD-82/92 — run a phase through its shell, with the phase-level wrapper
/// LAYERS applied around it. This is the single placement point for
/// `WrapperLevel::Phase` layers (the embryo of the per-level cascade): the
/// layers are generic `ExecShell` decorators, so the same types will wrap a
/// `ScenarioShell` here's-a-scenario-subject once scenario-level sources
/// land — no change to the layer itself.
///
/// Both phase dispatch sites (`execute_node`'s leaf and the comprehension's
/// `TerminalAction::Phase`) go through here so a layer can never be applied
/// at one site and missed at the other.
async fn run_phase_layered(
    ctx: &mut ExecCtx,
    name: &str,
    node_id: crate::scene_tree::SceneNodeId,
) -> crate::phase_outcome::Outcome {
    let leaf = PhaseShell::new(name, node_id);
    // `interval:` — resolve the schedule from the phase subject. `None` (the
    // default) means unscheduled: the leaf runs once, byte-for-byte as before.
    match crate::wrappers::interval::for_phase(&ctx.phases, &ctx.workload_params, name) {
        Some(spec) => {
            crate::wrappers::interval::IntervalShell::new(&leaf, spec, name)
                .run(ctx)
                .await
        }
        None => leaf.run(ctx).await,
    }
}

/// SRD-92 — the OP-leaf projection façade. The op leaf is an owned
/// per-worker producer BELOW the [`crate::phase_outcome::Outcome`]
/// projection boundary: it runs per-cycle with a borrowed cycle ctx
/// ([`crate::fixture::ExecCtx`]), so it is intentionally NOT an
/// [`ExecShell`] driven by `&mut ExecCtx` (Decision B / the leaf-as-
/// projection-boundary). This is the "map once at the bottom" — an op
/// dispenser's `Result<OpResult, ExecutionError>` projected to the
/// universal two-axis `Outcome`: a skip is `Skipped`, a clean result is
/// `Completed + Succeeded`, and a ran-and-errored op is `Completed + Failed`
/// (it *ran*; the result is untrustworthy — `Interrupted` is reserved for
/// cancelled-by-abort; the message rides `reason`). The op-specific payload
/// (`OpResult.body`) is **lifted into** the `Outcome` payload slot via
/// `OpBodyPayload` (SRD-92 Step 2). Consumed when the activity flows per-op
/// Outcomes (next steps); the `StanzaShell` (the per-cycle op chain) likewise
/// folds its ops' projections inside the activity rather than as a walker shell.
///
/// `OpBodyPayload` wraps the leaf's *erased* result body so it can ride the
/// payload slot as `Arc<dyn Payload>` — needed because the `ResultBody ->
/// Payload` bridge is a blanket impl (no `dyn`-upcast off a `Box<dyn
/// ResultBody>`). The body is moved in (no copy); one `Arc` alloc, only when a
/// body is present. Contents stay deferred (the data layer reads it later).
#[derive(Debug)]
#[allow(dead_code)] // WIP SRD-92.
struct OpBodyPayload(Box<dyn crate::adapter::ResultBody>);
impl crate::phase_outcome::Payload for OpBodyPayload {}

#[allow(dead_code)] // WIP SRD-92.
struct OpShell;

#[allow(dead_code)]
impl OpShell {
    fn project(
        res: Result<crate::adapter::OpResult, crate::adapter::ExecutionError>,
    ) -> crate::phase_outcome::Outcome {
        use crate::phase_outcome::Outcome;
        use std::sync::Arc;
        match res {
            Ok(r) if r.skipped => Outcome::skipped(),
            Ok(r) => match r.body {
                Some(body) => Outcome::completed().with_payload(Arc::new(OpBodyPayload(body))),
                None => Outcome::completed(),
            },
            Err(e) => Outcome::completed_failed().with_reason(e.to_string()),
        }
    }
}

#[cfg(test)]
mod srd92_leaf_shell_tests {
    use super::*;
    use crate::phase_outcome::{Disposition, Validity};

    #[test]
    fn op_shell_projects_quadrants_and_payload() {
        use crate::adapter::{AdapterError, ExecutionError, OpResult, TextBody};

        // clean, no body -> Completed+Succeeded, no payload
        let o = OpShell::project(Ok(OpResult::default()));
        assert_eq!(o.disposition, Disposition::Completed);
        assert_eq!(o.validity, Validity::Succeeded);
        assert!(o.payload.is_none());

        // skipped -> Skipped
        assert_eq!(
            OpShell::project(Ok(OpResult::skipped())).disposition,
            Disposition::Skipped
        );

        // ran-and-errored -> Completed+Failed (NOT Interrupted), message on reason
        let err = ExecutionError::Op(AdapterError {
            error_name: "test".into(),
            message: "boom".into(),
            retryable: false,
        });
        let o = OpShell::project(Err(err));
        assert_eq!(o.disposition, Disposition::Completed);
        assert_eq!(o.validity, Validity::Failed);
        assert!(o.reason.as_deref().unwrap_or_default().contains("boom"));

        // clean WITH a body -> payload populated
        let r = OpResult {
            body: Some(Box::new(TextBody("hi".into()))),
            skipped: false,
        };
        let o = OpShell::project(Ok(r));
        assert_eq!(o.disposition, Disposition::Completed);
        assert!(o.payload.is_some());
    }

    #[test]
    fn fold_aggregate_two_latch_quadrants() {
        // stop-policy fail: cascade reason + cut short -> Interrupted+Failed
        let o = fold_aggregate(Some("boom".into()), Some("boom".into()), false);
        assert_eq!(
            (o.disposition, o.validity),
            (Disposition::Interrupted, Validity::Failed)
        );
        assert_eq!(o.reason.as_deref(), Some("boom"));
        // NON-STOP fail (the fix): a failed child the handler let through, all
        // ran -> Completed+Failed (failure NOT lost), reason carried.
        let o = fold_aggregate(None, Some("soft".into()), false);
        assert_eq!(
            (o.disposition, o.validity),
            (Disposition::Completed, Validity::Failed)
        );
        assert_eq!(o.reason.as_deref(), Some("soft"));
        // workload halt, no failure -> Interrupted+Succeeded
        let o = fold_aggregate(None, None, true);
        assert_eq!(
            (o.disposition, o.validity),
            (Disposition::Interrupted, Validity::Succeeded)
        );
        // all clean -> Completed+Succeeded
        let o = fold_aggregate(None, None, false);
        assert_eq!(
            (o.disposition, o.validity),
            (Disposition::Completed, Validity::Succeeded)
        );
    }
}

/// Depth-tagged tree walk. Each recursive descent (into ForEach
/// children, ForCombinations children, DoWhile / DoUntil bodies,
/// or phase-level iterations) bumps `depth`. The `schedule_spec`
/// on `ctx` is consulted at `depth` to decide sibling strategy.
///
/// Per SRD 02 §"One Concurrency Path": there is only one dispatch
/// path — semaphore-gated `JoinSet`. `Bounded(1)` is the
/// sequential case (one permit at a time → spawn order = drain
/// order). No in-place serial loop; no special branch for
/// `nodes.len() <= 1`. The `concurrency_limit` is configuration
/// of the same harness, never a code branch.
fn execute_tree_at<'a>(
    ctx: &'a mut ExecCtx,
    nodes: &'a [ScenarioNode],
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>>
{
    Box::pin(async move {
        // SRD-92 — the scenario-graph shell's two-axis Outcome flows up
        // unchanged (no `Result` re-projection): a failing child's
        // disposition + validity + reason are preserved.
        let shell = ScenarioShell::scenario(nodes, depth);
        shell.dispatch(ctx, nodes, depth).await
    })
}

/// Spawn each sibling in its own task with a cloned `ExecCtx`.
/// Iter-var values flow exclusively via `ctx.current_parent_kernel`
/// per the M3.4b unified-comprehension contract — no separate
/// HashMap to clone. Bounded limits gate tasks through a
/// `Semaphore`; Unlimited launches all at once. Joins via
/// `JoinSet`; the first task error aborts the rest (remaining
/// tasks still finish their own permits).
/// SRD-82 — the scenario-graph shell's BODY + HANDLE + AGGREGATE. Runs
/// the sibling units via the one SRD-02 concurrency path (semaphore-gated
/// `JoinSet`, `Bounded(1)` = serial), feeds each joined child through
/// `handler` (the `*Failed:stop` scenario policy), and aggregates into a
/// two-axis [`crate::phase_outcome::Outcome`] + the first failure reason.
/// Wrapped by [`ScenarioShell`] / [`ExecShell`].
async fn run_scenario_body(
    handler: &ShellHandler,
    ctx: &mut ExecCtx,
    nodes: &[ScenarioNode],
    depth: usize,
) -> crate::phase_outcome::Outcome {
    use crate::scheduler::ConcurrencyLimit;
    let limit = ctx.schedule_spec.limit_at(depth);

    // Stable-order preview before spawning. Concurrent phases
    // race each other to the per-phase log entry, so without
    // this announcement the user sees `[2/2] phase 'right'`
    // before `[1/2] phase 'left'` (or the other way around)
    // depending on which task's `phase_starting` fires first.
    // Emit a single ordered line up-front so operators always
    // see the dispatch in declaration order, even if the per-
    // phase headers interleave below.
    //
    // Presentation-only guard: at `Bounded(1)` the dispatch IS
    // sequential (one permit, JoinSet drains in spawn order =
    // declaration order), so the per-phase headers come out in
    // order naturally and the preview adds nothing. Skip it. This
    // is NOT a code-path branch (the dispatch loop below is
    // identical for every limit per SRD 02 §"One Concurrency
    // Path"); it's just suppressing a log line that conveys the
    // wrong intent ("concurrent dispatch (limit=1)" reads as
    // parallelism when there is none).
    let preview_useful = !matches!(limit, ConcurrencyLimit::Bounded(1));
    if preview_useful {
        let scheduled_phases: Vec<(usize, String)> = nodes
            .iter()
            .filter_map(|node| match node {
                ScenarioNode::Phase(name) => Some(name.clone()),
                _ => None,
            })
            .filter_map(|name| {
                crate::scene_tree::current().and_then(|t| {
                    t.dfs_phases()
                        .find(|n| n.name == name)
                        .and_then(|n| n.seq)
                        .map(|seq| (seq, name.clone()))
                })
            })
            .collect();
        if !scheduled_phases.is_empty() {
            let limit_disp = match limit {
                ConcurrencyLimit::Bounded(n) => format!("limit={n}"),
                ConcurrencyLimit::Unlimited => "limit=*".to_string(),
            };
            let total = crate::scene_tree::current()
                .map(|t| t.total_phases())
                .unwrap_or(scheduled_phases.len());
            let listing: Vec<String> = scheduled_phases
                .iter()
                .map(|(seq, name)| format!("[{seq}/{total}] {name}"))
                .collect();
            crate::diag!(
                crate::observer::LogLevel::Info,
                "concurrent dispatch ({limit_disp}): {}",
                listing.join(", ")
            );
        }
    }

    let sem: Option<Arc<tokio::sync::Semaphore>> = match limit {
        ConcurrencyLimit::Bounded(n) => Some(Arc::new(tokio::sync::Semaphore::new(n as usize))),
        ConcurrencyLimit::Unlimited => None,
    };
    // Dispatch is serialised on the deterministic
    // declaration-order of scenario nodes; execution is
    // concurrent. The permit acquire happens HERE in the
    // dispatcher loop (not inside the spawned task), so:
    //
    //   - With `Bounded(N)`: at most N tasks in-flight; the
    //     loop blocks before spawning the (N+1)th until one of
    //     the live ones completes (releases its permit). The
    //     (N+1)th is then the next-in-declaration-order
    //     scenario node.
    //   - With `Unlimited`: no semaphore — spawn order is
    //     declaration order, which is what the user-visible
    //     dispatch order needs to be regardless of how the
    //     tokio runtime schedules the spawned tasks.
    //   - With `Serial`: the caller already routed to the
    //     non-concurrent loop above; we never reach here.
    // Positional scope-tree resolution (One Walker). Each scenario node maps
    // 1:1, in declaration order, to a child of the current scope-tree node
    // (the parent cursor) — `append_subtree` pushes exactly one scope node per
    // scenario node. Resolving each node's scope index by POSITION (not by AST
    // match) is what lets AST-identical sibling comprehensions / bindings
    // disambiguate; the old content-keyed lookup returned the first match for
    // both (the "task #19" drift bug).
    let parent_scope_idx = ctx.current_scope_idx;
    let child_scope_indices: Vec<crate::scope_tree::ScopeNodeIdx> =
        ctx.scope_tree.nodes[parent_scope_idx].children.clone();
    if child_scope_indices.len() != nodes.len() {
        return crate::phase_outcome::Outcome::failed().with_reason(format!(
            "scope-tree/scenario-tree drift: scope node {parent_scope_idx} has {} \
             child scopes but the walker is dispatching {} sibling scenario nodes",
            child_scope_indices.len(),
            nodes.len(),
        ));
    }

    // SRD-82/83 — if the walk has already halted (a prior phase failed,
    // or a stop condition tripped — possibly in another for-loop
    // iteration or sibling subtree), run NOTHING in this scope: neither
    // foreground phases NOR daemons. The foreground loop's per-node
    // `should_stop` check below would skip the foreground, but daemons
    // are spawned before it — so without this guard the comprehension
    // keeps iterating after a halt and every iteration still spawns its
    // daemon phase(s). Returns a clean Interrupt; the failing branch
    // carries the fault up its own `Err`.
    if ctx.workload_shell.should_stop() {
        return crate::phase_outcome::Outcome::interrupted();
    }

    // SRD-82 Part 6 — partition the body into FOREGROUND and DAEMON
    // units. A daemon phase runs concurrently with the foreground
    // siblings, OFF the foreground concurrency budget, and is stopped
    // when the foreground completes. Daemons are spawned first, onto
    // their own `JoinSet` and without a foreground permit, so they are up
    // for the foreground's whole duration; the foreground loop skips them.
    let daemon_flags: Vec<bool> = nodes
        .iter()
        .map(|n| match n {
            ScenarioNode::Phase(name) => ctx.phases.get(name).map(|p| p.daemon).unwrap_or(false),
            _ => false,
        })
        .collect();
    let any_daemon = daemon_flags.iter().any(|&d| d);
    let daemon_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut daemon_set = tokio::task::JoinSet::new();
    for (i, node) in nodes.iter().enumerate() {
        if !daemon_flags[i] {
            continue;
        }
        // SRD-100 — a daemon is DISPATCHED ahead of its foreground siblings
        // (spawned here, before the foreground loop below), so its scene
        // node must also be PUSHED ahead of them: `seq` is assigned at push
        // time (scene_tree: count-of-phases + 1), and a foreground sibling's
        // async push would otherwise land first and steal the lower seq —
        // making the daemon sort/display AFTER a phase it actually precedes.
        // Push the daemon's leaf scene node synchronously NOW, in dispatch
        // order; the later `execute_node` push is idempotent (find-or-create
        // by (parent, name)) and resolves to this same node + seq. (Mirrors
        // the leaf-phase push in `execute_node`'s `ScenarioNode::Phase` arm —
        // keep the label/path derivation in sync. A for_each daemon is
        // skipped: its per-iter cells are pushed asynchronously by the
        // comprehension dispatcher, not here.)
        if let ScenarioNode::Phase(dname) = node
            && ctx
                .phases
                .get(dname.as_str())
                .map(|p| p.for_each.is_none())
                .unwrap_or(true)
        {
            let op_names: Vec<String> = ctx
                .phases
                .get(dname.as_str())
                .map(|p| p.ops.iter().map(|op| op.name.clone()).collect())
                .unwrap_or_default();
            let phase_labels = canonical_phase_label(
                &ctx.current_parent_kernel
                    .as_ref()
                    .map(|k| {
                        k.scope_coordinates()
                            .iter()
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            );
            let mut phase_path = ctx.scene_tree_path.clone();
            phase_path.push(crate::checkpoint::PathSegment::Phase(dname.clone()));
            let _ = push_phase_scene_node(
                ctx.scene_tree_parent_id,
                phase_path,
                dname.clone(),
                phase_labels,
                op_names,
            );
        }
        let node_scope_idx = child_scope_indices[i];
        let node = node.clone();
        let mut task_ctx = ctx.clone();
        // The daemon-group completion flag — `run_phase` threads it onto
        // the daemon phase's `Activity::daemon_stop`.
        task_ctx.daemon_stop = Some(daemon_stop.clone());
        daemon_set.spawn(crate::execution_context::propagate(async move {
            execute_node(&mut task_ctx, &node, node_scope_idx, depth).await
        }));
    }

    let mut set = tokio::task::JoinSet::new();
    // The first child the handler decided to STOP on (its reason). `Outcome`
    // carries no message, so the reason rides alongside.
    let mut first_failure: Option<String> = None;
    // SRD-92 two-latch fold — the validity latch: the first FAILED child's
    // reason, set independent of the handler's stop decision.
    let mut any_failed_reason: Option<String> = None;
    // SRD-92 Step 5d — the foreground children now come through the unified
    // `ChildSource` contract: a realized, distinct-sub-unit list → `Realizable`
    // → `select_drive` = `BoundedSpawn` (this very JoinSet path). `poll_next`
    // yields `Node(i)` in declaration order — behaviour-identical to the old
    // `nodes.iter().enumerate()`; the walker resolves `nodes[i]` by position.
    use crate::child_source::{Child, ChildSource, CountedSource, Drive, select_drive};
    let mut foreground = CountedSource::new(nodes.len());
    debug_assert_eq!(
        select_drive(foreground.realizability()),
        Drive::BoundedSpawn
    );
    while let Some(Child::Node(i)) = foreground.poll_next() {
        if daemon_flags[i] {
            continue;
        } // daemons already spawned off-budget
        let node = &nodes[i];
        let node_scope_idx = child_scope_indices[i];
        let permit = match sem.as_ref() {
            Some(s) => match s.clone().acquire_owned().await {
                Ok(p) => Some(p),
                // The semaphore is owned here and never closed, so this is
                // effectively unreachable — surface it as a fault rather
                // than panicking if it ever does.
                Err(e) => {
                    return crate::phase_outcome::Outcome::failed().with_reason(e.to_string());
                }
            },
            None => None,
        };
        // After waiting for a permit, drain any completed
        // tasks and check for errors. Cascade-stop preserves
        // the semantic that an erroring sibling halts dispatch
        // of subsequent siblings (matches the in-place serial
        // loop's `?`-propagation behaviour). At `Bounded(1)`
        // the permit acquire above blocks until the previous
        // task is fully done; at `Bounded(N)` up to N-1 tasks
        // may already be in flight when we see the first
        // error — those continue to completion, but no further
        // dispatch happens. Same loop body for every limit;
        // the only difference is how many in-flight tasks the
        // cap allows.
        while let Some(res) = set.try_join_next() {
            // HANDLE + two-latch fold: cascade-stop decision + validity latch.
            fold_child(res, handler, &mut first_failure, &mut any_failed_reason);
        }
        if first_failure.is_some() {
            // Drop the permit we just acquired — it would
            // otherwise sit unused until the dispatch loop
            // exits.
            drop(permit);
            break;
        }
        // SRD-83 — workload-shell stop. A stop condition tripped at
        // some prior phase outcome (anywhere in the tree) latched the
        // shell's `walk_stop`; halt dispatch of the remaining siblings.
        // This is the broad halt the local `first_err` cascade can't
        // give — it reaches every dispatch loop at every depth, so a
        // trip in one subtree stops not-yet-started phases in sibling
        // subtrees too (the scenario stop-on-error default). In-flight
        // `Bounded(N>1)` siblings already running abort cooperatively
        // too (SRD-82 Part 4): their fibers poll the same per-execution
        // `walk_stop` flag (`Activity::walk_stop`) at their boundaries
        // and exit rather than draining — so this check halts the
        // not-yet-dispatched ones and the in-flight ones unwind in
        // parallel.
        if ctx.workload_shell.should_stop() {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "workload shell stopped — halting dispatch of remaining \
                 siblings at depth {depth}"
            );
            drop(permit);
            break;
        }
        let node = node.clone();
        let mut task_ctx = ctx.clone();
        // SRD-88: carry the per-execution context across the spawn boundary so
        // this fiber resolves to ITS execution's observer / scene tree / stop
        // flag (a `tokio` task doesn't inherit the parent's task-locals).
        // No-op for the single-run path (no context scoped — A1).
        set.spawn(crate::execution_context::propagate(async move {
            // Permit moves into the task; dropped when the
            // task body returns, freeing a slot for the next
            // dispatch iteration above.
            let _permit = permit;
            execute_node(&mut task_ctx, &node, node_scope_idx, depth).await
        }));
    }
    // Drain any still-running tasks (those spawned before the stop was
    // observed), feeding each through the same handler.
    while let Some(res) = set.join_next().await {
        fold_child(res, handler, &mut first_failure, &mut any_failed_reason);
    }
    // SRD-82 Part 6 — the foreground body has finished: latch the daemon
    // group's completion flag (each daemon phase's fibers poll it and
    // exit within their cancel grace), then drain the daemons. A
    // cleanly-stopped daemon is `Completed` → Continue; a daemon that
    // ERRORED folds through the same handler and bubbles up as a failure.
    if any_daemon {
        daemon_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        while let Some(res) = daemon_set.join_next().await {
            fold_child(res, handler, &mut first_failure, &mut any_failed_reason);
        }
    }
    // AGGREGATE: SRD-92 two-latch fold (disposition × validity independent).
    fold_aggregate(
        first_failure,
        any_failed_reason,
        ctx.workload_shell.should_stop(),
    )
}

// ─── SceneTree push helpers ───────────────────────────────────────
//
// Per SRD 18b §"Single Walker Contract" point 2: every walker arm
// pushes its scene-tree node as part of its structural work. These
// helpers encapsulate the global-mutex dance + auxiliary setters
// (op_names / own_names / yaml_path) so each arm reads as the
// minimal "what does this arm push" statement.
//
// The walker is the SOLE populator of `crate::scene_tree::current()`
// — there is no separate pre-map walk that pre-populates the tree
// (SRD 18b §"Single Walker Contract" point 1). At depth >= Cycle
// the walker descends both structural (push) and executional
// (run_phase) work; at depth = Phase the walker still pushes every
// node but skips the execution at run_phase's internal short-circuit.

fn push_phase_scene_node(
    parent_id: crate::scene_tree::SceneNodeId,
    yaml_path: Vec<crate::checkpoint::PathSegment>,
    name: String,
    labels: String,
    op_names: Vec<String>,
) -> crate::scene_tree::SceneNodeId {
    let mut id: crate::scene_tree::SceneNodeId = 0;
    crate::scene_tree::with_global_mut(|t| {
        id = t.push(parent_id, crate::scene_tree::NodeKind::Phase, name, labels);
        t.set_phase_op_names(id, op_names);
        t.set_yaml_path(id, yaml_path);
    });
    id
}

fn push_scope_scene_node(
    parent_id: crate::scene_tree::SceneNodeId,
    yaml_path: Vec<crate::checkpoint::PathSegment>,
    header: String,
    own_names: Vec<String>,
) -> crate::scene_tree::SceneNodeId {
    let mut id: crate::scene_tree::SceneNodeId = 0;
    crate::scene_tree::with_global_mut(|t| {
        id = t.push(
            parent_id,
            crate::scene_tree::NodeKind::Scope,
            header,
            String::new(),
        );
        if !own_names.is_empty() {
            t.set_own_names(id, own_names);
        }
        t.set_yaml_path(id, yaml_path);
    });
    id
}

/// Format a per-iter binding tuple as `k=v, k=v`. Same shape
/// pre-map used; surface visible in TUI / post-run summary.
fn format_iter_label(bindings: &[(String, polydat::ast::Value)]) -> String {
    bindings
        .iter()
        .map(|(k, v)| format!("{k}={}", v.to_display_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format the canonical phase label from a root-first coord chain.
/// Reverses to leaf-first and runs the Polydat-side formatter — same
/// string the runtime's `phase_labels` produces, so scene-tree
/// `find_phase` matches at execution time.
fn canonical_phase_label(parent_coords: &[ScopeCoord]) -> String {
    let leaf_first: Vec<_> = parent_coords.iter().rev().cloned().collect();
    format_scope_coordinate_path(&leaf_first)
}

/// Look up the own-output names of a do-loop's installed kernel by
/// matching the scope-tree DoWhile/DoUntil variant on
/// (condition, counter). Used by the walker's DoWhile/DoUntil
/// arms to seed `own_names` on the pushed scene-tree node so
/// downstream consumers can render the loop's own coordinates.
fn do_loop_own_names(
    ctx: &ExecCtx,
    condition: &str,
    counter: Option<&str>,
    invert: bool,
) -> Vec<String> {
    let idx = ctx
        .scope_tree
        .iter_dfs()
        .find_map(|(idx, n)| match &n.kind {
            crate::scope_tree::ScopeKind::DoWhile {
                condition: c,
                counter: ct,
            } if !invert && c == condition && ct.as_deref() == counter => Some(idx),
            crate::scope_tree::ScopeKind::DoUntil {
                condition: c,
                counter: ct,
            } if invert && c == condition && ct.as_deref() == counter => Some(idx),
            _ => None,
        });
    idx.and_then(|i| ctx.scope_tree.nodes[i].cached_kernel.get().cloned())
        .map(|k| {
            k.program()
                .own_output_names()
                .into_iter()
                .map(String::from)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
}

/// Resolve the parent kernel for a scope dispatched at runtime.
///
/// `ctx.current_parent_kernel` carries the *live execution* kernel
/// of the immediately-enclosing scope — set by `run_one_iteration`
/// (or the do-loop dispatcher) to its per-iteration kernel before
/// descending into children, restored on the way out. That's the
/// kernel that has the outer iter-var values *set to the current
/// iteration's values*, so spec evaluation in nested scopes sees
/// `{outer_var}` resolve correctly.
///
/// The scope tree's canonical ancestor kernel (via
/// `nearest_installed_ancestor_kernel`) only carries the structural
/// constants; iter vars are present as outputs but their values are
/// the kernel's defaults, not the current iteration's values.
///
/// Prefer the live execution kernel; fall back to the structural
/// ancestor when no dispatcher is currently active above us.
fn effective_parent_kernel(
    ctx: &ExecCtx,
    scope_idx: usize,
) -> Option<std::sync::Arc<polydat::kernel::PolydatKernel>> {
    ctx.current_parent_kernel
        .clone()
        .or_else(|| ctx.scope_tree.nearest_installed_ancestor_kernel(scope_idx))
}

/// Execute a single scenario node. Descent into children happens
/// at `depth + 1`; iteration loops (ForEach, ForCombinations,
/// phase-level for_each, DoWhile, DoUntil) also treat their
/// iteration instances as siblings at `depth + 1` and honor the
/// concurrency limit at that depth.
/// Whether `node` contains at least one phase that matches the
/// supplied phase-name pattern. Used by [`execute_node`] to
/// elide scope subtrees with no active leaves (per the user's
/// "any branch with no active leaf nodes should be disabled"
/// rule). Walks the workload-model scenario tree directly —
/// no scene-tree dependency, so it's safe to consult before
/// `push_scope_scene_node` would fire.
/// SRD-106 — deliberately NOT prereq-aware: scope liveness is decided
/// by SELECTED phases only. If it also counted `checkpoint: idempotent`
/// prereqs, an unselected sibling section would drag its own prereq
/// chain into execution (e.g. every per-profile load in a grid the
/// operator didn't ask for). The prereq exemption applies at the
/// phase-leaf gate below, inside scopes this rule keeps alive.
fn subtree_has_active_phase(
    node: &ScenarioNode,
    pattern: &crate::phase_filter::PhasePattern,
) -> bool {
    match node {
        ScenarioNode::Phase(name) => pattern.is_match(name),
        ScenarioNode::Comprehension { children, .. }
        | ScenarioNode::DoWhile { children, .. }
        | ScenarioNode::DoUntil { children, .. }
        | ScenarioNode::IncludedScenario { children, .. } => children
            .iter()
            .any(|c| subtree_has_active_phase(c, pattern)),
        // Non-phase, non-scope nodes (Bindings, etc.) carry no
        // phases themselves — treat as inactive so a branch
        // containing only them gets pruned. Sibling branches
        // are evaluated independently.
        _ => false,
    }
}

fn execute_node<'a>(
    ctx: &'a mut ExecCtx,
    node: &'a ScenarioNode,
    // This node's own scope-tree index, resolved positionally by the caller
    // (`run_siblings_concurrently`). Descending arms set it as the scope cursor
    // for their children; the Comprehension / Bindings arms also use it to find
    // their installed kernel — no AST lookup (One Walker positional resolution).
    node_scope_idx: crate::scope_tree::ScopeNodeIdx,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>>
{
    Box::pin(async move {
        use crate::checkpoint::PathSegment;
        // Scope-elision gate: if a phases= filter is set and
        // NO descendant phase in this subtree matches, skip the
        // whole node. Phase-arm leaf nodes still go through the
        // arm-local active gate below so the scene tree push
        // happens; scope nodes can be skipped entirely here
        // because their only purpose is to wrap descendant
        // phases.
        if let Some(pat) = ctx.phase_filter.clone() {
            let is_scope = !matches!(node, ScenarioNode::Phase(_));
            if is_scope && !subtree_has_active_phase(node, &pat) {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "phases=<filter>: eliding scope (no descendant phase matches)"
                );
                return crate::phase_outcome::Outcome::skipped();
            }
        }
        match node {
            ScenarioNode::Phase(name) => {
                let phase_fe = ctx
                    .phases
                    .get(name.as_str())
                    .and_then(|p| p.for_each.clone());
                let op_names: Vec<String> = ctx
                    .phases
                    .get(name.as_str())
                    .map(|p| p.ops.iter().map(|op| op.name.clone()).collect())
                    .unwrap_or_default();
                if let Some(spec) = phase_fe {
                    // Phase-level for_each routes through the
                    // unified dispatcher with terminal action
                    // `TerminalAction::Phase(name)` — the scope
                    // kernel exposes the iter var as a scope
                    // output, the dispatcher's per-branch kernel
                    // is set as the parent for the leaf phase
                    // compile, the phase runs once per iter
                    // value.
                    let scope_idx = match ctx.scope_tree.phase_node_by_name(name) {
                        Some(v) => v,
                        None => {
                            return crate::phase_outcome::Outcome::failed().with_reason(format!(
                                "phase '{name}' for_each '{spec}': no matching scope-tree entry."
                            ));
                        }
                    };
                    let canonical =
                        match ctx.scope_tree.nodes[scope_idx].cached_kernel.get().cloned() {
                            Some(v) => v,
                            None => return crate::phase_outcome::Outcome::failed().with_reason(
                                format!(
                                    "phase '{name}' for_each '{spec}': scope at index {scope_idx} \
                             has no installed phase-for_each kernel."
                                ),
                            ),
                        };
                    let parent = match effective_parent_kernel(ctx, scope_idx) {
                        Some(v) => v,
                        None => {
                            return crate::phase_outcome::Outcome::failed().with_reason(format!(
                                "phase '{name}' for_each '{spec}': no installed ancestor kernel."
                            ));
                        }
                    };
                    // Delegate the for_each grammar to polydat (the single
                    // owner): `parse_inline` builds the algebra comprehension for
                    // single- OR multi-clause specs (e.g. "batch in [1,2], conc in
                    // [8,16]"), identical to the scenario-level for_each path.
                    let comprehension =
                        match polydat::iteration::comprehension::spec::parse_inline(&spec) {
                            Ok(v) => v,
                            Err(e) => {
                                return crate::phase_outcome::Outcome::failed()
                                    .with_reason(format!("phase '{name}' for_each '{spec}': {e}"));
                            }
                        };
                    let iter_vars: Vec<String> = comprehension
                        .coordinate_specs()
                        .into_iter()
                        .map(|(v, _)| v)
                        .collect();
                    // Joined display/identity label for the scope (one var in the
                    // common case; multi-clause joins them).
                    let var_label = iter_vars.join(", ");
                    let needle = spec.clone();
                    let parent_coords = ctx
                        .current_parent_kernel
                        .as_ref()
                        .map(|k| {
                            k.scope_coordinates()
                                .iter()
                                .rev()
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let optimize_block = ctx
                        .phases
                        .get(name.as_str())
                        .and_then(|p| p.optimize.clone());
                    // A continuous optimize axis (`x in lo .. hi`, float range) is
                    // SAMPLED by the optimizer — read its bounds from the
                    // comprehension and skip enumeration (a continuous source
                    // yields no tuples; the optimizer IS the sampling strategy,
                    // so V8's order-requirement never applies here).
                    let continuous_axes = optimize_block
                        .as_ref()
                        .and_then(|_| continuous_axis_intervals(&comprehension));
                    let steps = if continuous_axes.is_some() {
                        Vec::new()
                    } else {
                        match runtime_iterate(
                            ctx,
                            &canonical,
                            &parent,
                            &parent_coords,
                            &comprehension,
                        ) {
                            Ok(v) => v,
                            Err(e) => {
                                return crate::phase_outcome::Outcome::failed()
                                    .with_reason(enrich_with_yaml_location(ctx, &needle, e));
                            }
                        }
                    };
                    // Structural push: outer phase.for_each scope
                    // header. Per SRD 18b §"Single Walker Contract"
                    // point 2 — runs at every depth.
                    let mut scope_path = ctx.scene_tree_path.clone();
                    scope_path.push(PathSegment::ForEach {
                        var: var_label.clone(),
                    });
                    let header = if let Some(b) = &optimize_block {
                        // SRD-86 A12 — a search node, not a sweep: its spec and
                        // budget, never an enumerated coordinate set. A continuous
                        // axis is sampled, so it has no enumerated steps to read a
                        // name from — use the clause variable directly.
                        let axes = if continuous_axes.is_some() {
                            var_label.clone()
                        } else {
                            steps
                                .first()
                                .map(|s| {
                                    s.bindings
                                        .iter()
                                        .map(|(k, _)| k.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default()
                        };
                        format!(
                            "search · {} · maximize {} · {{{axes}}} · ≤{} evals",
                            b.method, b.objective, b.max_evals
                        )
                    } else {
                        format!(
                            "phase.for_each {var_label} in [{}]",
                            steps
                                .iter()
                                .filter_map(|s| s
                                    .bindings
                                    .first()
                                    .map(|(_, v)| v.to_display_string()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let scope_id = push_scope_scene_node(
                        ctx.scene_tree_parent_id,
                        scope_path.clone(),
                        header,
                        Vec::new(),
                    );
                    // dispatch_comprehension pushes per-iter Phase
                    // nodes under this scope (TerminalAction::Phase).
                    // Save+update ctx.scene_tree_* so iterations land
                    // under the outer scope; restore on the way out.
                    let saved_parent = ctx.scene_tree_parent_id;
                    let saved_path = std::mem::replace(&mut ctx.scene_tree_path, scope_path);
                    ctx.scene_tree_parent_id = scope_id;
                    let phase_path_for_iters: Vec<PathSegment> = {
                        let mut p = ctx.scene_tree_path.clone();
                        p.push(PathSegment::Phase(name.clone()));
                        p
                    };
                    let res = if let Some(b) = optimize_block {
                        // Continuous axes are sampled from their bounds; discrete
                        // axes carry feasibility (holes) in the enumerated grid.
                        let (space, coord_eval) = if let Some(intervals) = continuous_axes {
                            let names = iter_vars.clone();
                            (
                                search_space_continuous(&names, &intervals),
                                CoordEval::Synthesized {
                                    axis_names: names,
                                    canonical: canonical.clone(),
                                    parent: parent.clone(),
                                    parent_coords: parent_coords.clone(),
                                },
                            )
                        } else {
                            let space = search_space_from_steps(&steps);
                            let index = index_steps(&steps);
                            (space, CoordEval::Enumerated { steps, index })
                        };
                        dispatch_optimization(
                            ctx,
                            space,
                            coord_eval,
                            b,
                            name,
                            depth + 1,
                            Some((name.clone(), op_names, phase_path_for_iters)),
                        )
                        .await
                    } else {
                        // SRD-101 sweep gate (phase-level `for_each`) — compile
                        // the gate against the sweep's parent scope before
                        // `name` is moved into the terminal.
                        let phase_continue_if = ctx
                            .phases
                            .get(name.as_str())
                            .and_then(|p| p.continue_if.clone());
                        let coord_sample =
                            steps.first().map(|s| s.bindings.as_slice()).unwrap_or(&[]);
                        let gate = match resolve_continue_if(
                            phase_continue_if,
                            &parent,
                            coord_sample,
                            ctx.strict,
                        ) {
                            Ok(g) => g,
                            Err(e) => {
                                return crate::phase_outcome::Outcome::failed().with_reason(e);
                            }
                        };
                        dispatch_comprehension(
                            ctx,
                            steps,
                            TerminalAction::Phase(name),
                            depth + 1,
                            false,
                            "for_each",
                            Some((name.clone(), op_names, phase_path_for_iters)),
                            gate,
                        )
                        .await
                    };
                    ctx.scene_tree_parent_id = saved_parent;
                    ctx.scene_tree_path = saved_path;
                    return enrich_outcome(ctx, &needle, res); // SRD-92: propagate the real Outcome, yaml-enriched
                } else {
                    // Structural push: leaf Phase node. Labels are
                    // the canonical leaf-first scope-coord path so
                    // run_phase's later `set_phase_running(name,
                    // &phase_labels, ..)` find_phase lookup matches.
                    let phase_labels = canonical_phase_label(
                        &ctx.current_parent_kernel
                            .as_ref()
                            .map(|k| {
                                k.scope_coordinates()
                                    .iter()
                                    .rev()
                                    .cloned()
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                    );
                    let mut phase_path = ctx.scene_tree_path.clone();
                    phase_path.push(PathSegment::Phase(name.clone()));
                    // Clone `phase_labels` before moving it into
                    // the scene-tree push — the gate below still
                    // needs to match it against the refine plan.
                    let phase_labels_for_gate = phase_labels.clone();
                    // SRD-100 P1c — capture this leaf phase's own
                    // scene-node id; it's the dispatch-time row key
                    // threaded into the sentinel flips + `run_phase`
                    // so lifecycle routing never re-matches by name.
                    let phase_node_id = push_phase_scene_node(
                        ctx.scene_tree_parent_id,
                        phase_path,
                        name.clone(),
                        phase_labels,
                        op_names,
                    );
                    let phase_labels = phase_labels_for_gate;
                    // Depth gating per SRD 17 §"Execution Depth"
                    // + SRD 18b §"Single Walker Contract" point
                    // 3: structural push always runs; executional
                    // run_phase only runs at depth >= Op. run_phase's
                    // internal short-circuit then handles
                    // Op-vs-Cycle-vs-Full at the cycle boundary.
                    // phases=<pattern> gate: skip execution when
                    // the pattern was set and this phase's name
                    // doesn't match. The structural push above
                    // still ran so the tree / TUI / coords stay
                    // intact; only the per-cycle work is elided.
                    //
                    // SRD-106 — prereq exemption: a phase declared
                    // `checkpoint: idempotent` is upstream state
                    // production whose at-most-once execution is
                    // governed by PROVENANCE (the resume / refine
                    // gates below), not by selection. Within a live
                    // scope it stays in the walk even when the
                    // filter doesn't name it. Scope liveness itself
                    // comes from selected phases only — see
                    // `subtree_has_active_phase`.
                    let is_prereq_class = ctx
                        .phases
                        .get(name)
                        .and_then(|p| p.checkpoint.as_ref())
                        .map(|c| c.idempotent)
                        .unwrap_or(false);
                    let prereq_exempt = ctx.phase_filter.is_some() && is_prereq_class;
                    let pattern_active = ctx
                        .phase_filter
                        .as_ref()
                        .map(|pat| pat.is_match(name) || prereq_exempt)
                        .unwrap_or(true);
                    if prereq_exempt
                        && !ctx
                            .phase_filter
                            .as_ref()
                            .map(|pat| pat.is_match(name))
                            .unwrap_or(true)
                    {
                        crate::diag!(
                            crate::observer::LogLevel::Info,
                            "phases=<filter>: phase '{name}' kept as an \
                             idempotent prerequisite (checkpoint: idempotent) \
                             — resume/refine provenance decides skip-or-run"
                        );
                    }
                    // SRD-77 refine `--scope=missing` fast-path:
                    // for the default scope, we can skip without
                    // computing the phase hash. `Changed` mode
                    // needs the hash, so it falls through into
                    // `run_phase` where the hash is computed and
                    // a deferred skip-gate evaluates there.
                    //
                    // SRD-106 carve-outs:
                    // - A phase the `phases=` filter NAMES is explicit
                    //   intent to run: selection defeats the skip.
                    // - A `checkpoint: idempotent` prereq never takes
                    //   the no-hash fast path — its skip validity
                    //   requires hash equality (a stale load must
                    //   re-run), so it defers to the run_phase hash
                    //   gate exactly like scope=changed.
                    let explicitly_selected = ctx
                        .phase_filter
                        .as_ref()
                        .map(|pat| pat.is_match(name))
                        .unwrap_or(false);
                    let refine_missing_skip = ctx
                        .refine_plan
                        .as_ref()
                        .filter(|p| p.scope == crate::refine_plan::RefineScope::Missing)
                        .map(|p| p.is_completed(name, &phase_labels))
                        .unwrap_or(false)
                        && !explicitly_selected
                        && !is_prereq_class;
                    if !pattern_active {
                        crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "phases=<filter>: skipping phase '{name}' (does not match)"
                        );
                        // Mark the deliberate skip on the same observer +
                        // scene-tree surfaces every other skip path uses
                        // (checkpoint resume, refine scope=missing): a
                        // zero-duration completion. Without this the node
                        // stays Pending and the post-run unreached-phase
                        // guard reports a filtered leaf as stranded
                        // (warning + exit 2). Suppressed under
                        // `pre_map_only` for the same reason as the
                        // dryrun=phase branch below.
                        if !ctx.pre_map_only {
                            crate::scene_tree::with_global_mut(|t| {
                                t.set_phase_running_at(phase_node_id, 0);
                                t.set_phase_completed_at(phase_node_id, 0.0);
                            });
                            ctx.observer.phase_starting(
                                phase_node_id,
                                name,
                                &phase_labels,
                                0,
                                0,
                                0,
                            );
                            ctx.observer
                                .phase_completed(phase_node_id, name, &phase_labels, 0.0);
                            crate::phase_end_triggers::fire_phase_completed(
                                name,
                                &phase_labels,
                                0.0,
                            );
                        }
                    } else if ctx.diag.depth < crate::runner::ExecDepth::Dispenser {
                        // `dryrun=phase`: structural walk only —
                        // no `run_phase` call, no dispensers built.
                        // Fire the sentinel phase_completed so the
                        // scene tree transitions Running → Completed
                        // and the post-run summary shows `[ok]`.
                        //
                        // Suppressed under `pre_map_only` — the
                        // pre-map structural pass walks the same
                        // depth as `dryrun=phase` but ISN'T
                        // execution: "completion" is undefined
                        // there and leaking the Completed status
                        // makes the TUI margin read `N/N` 50 ms
                        // into the run.
                        if !ctx.pre_map_only {
                            crate::scene_tree::with_global_mut(|t| {
                                t.set_phase_running_at(phase_node_id, 0);
                                t.set_phase_completed_at(phase_node_id, 0.0);
                            });
                            ctx.observer.phase_starting(
                                phase_node_id,
                                name,
                                &phase_labels,
                                0,
                                0,
                                0,
                            );
                            ctx.observer
                                .phase_completed(phase_node_id, name, &phase_labels, 0.0);
                            crate::phase_end_triggers::fire_phase_completed(
                                name,
                                &phase_labels,
                                0.0,
                            );
                        }
                    } else if refine_missing_skip {
                        // SRD-77 refine `scope=missing` skip:
                        // prior outcome exists, no hash check
                        // needed. Mark Completed (zero duration)
                        // on the scene tree + observer so the
                        // post-run summary shows `ok`.
                        crate::diag!(
                            crate::observer::LogLevel::Info,
                            "refine: skipping phase '{name}' [{phase_labels}] \
                             (prior completed outcome)"
                        );
                        crate::scene_tree::with_global_mut(|t| {
                            t.set_phase_running_at(phase_node_id, 0);
                            t.set_phase_completed_at(phase_node_id, 0.0);
                        });
                        ctx.observer
                            .phase_starting(phase_node_id, name, &phase_labels, 0, 0, 0);
                        ctx.observer
                            .phase_completed(phase_node_id, name, &phase_labels, 0.0);
                        crate::phase_end_triggers::fire_phase_completed(name, &phase_labels, 0.0);
                    } else if ctx.diag.depth >= crate::runner::ExecDepth::Dispenser {
                        // Falls through for `scope=changed` (needs
                        // the hash, computed inside run_phase) and
                        // for non-refine runs.
                        let __o = run_phase_layered(ctx, name, phase_node_id).await;
                        if __o.is_failure() {
                            return __o; // SRD-92: propagate the phase's REAL Outcome (no round-trip)
                        }
                    }
                }
            }
            ScenarioNode::Comprehension {
                comprehension,
                children,
                continue_if,
                anchor: _,
            } => {
                let label = crate::scope_tree::ScopeKind::Comprehension {
                    comprehension: comprehension.clone(),
                }
                .label();
                // Positional resolution (One Walker): the dispatcher already
                // mapped this scenario node to its scope-tree node by position,
                // so AST-identical sibling comprehensions disambiguate (the old
                // content-keyed `find_comprehension_scope_under` returned the
                // first match for both — the "task #19" drift bug). The
                // debug-assert pins the scenario↔scope-tree alignment.
                let scope_idx = node_scope_idx;
                debug_assert!(
                    matches!(
                        &ctx.scope_tree.nodes[scope_idx].kind,
                        crate::scope_tree::ScopeKind::Comprehension { comprehension: c }
                            if c == comprehension
                    ),
                    "{label}: positional scope node {scope_idx} is not the matching comprehension",
                );
                let canonical = match ctx.scope_tree.nodes[scope_idx].cached_kernel.get().cloned() {
                    Some(v) => v,
                    None => {
                        return crate::phase_outcome::Outcome::failed().with_reason(format!(
                            "{label}: scope at index {scope_idx} has no installed kernel.",
                        ));
                    }
                };
                let parent = match effective_parent_kernel(ctx, scope_idx) {
                    Some(v) => v,
                    None => {
                        return crate::phase_outcome::Outcome::failed()
                            .with_reason(format!("{label}: no installed ancestor kernel.",));
                    }
                };
                let own_names: Vec<String> = canonical
                    .program()
                    .own_output_names()
                    .into_iter()
                    .map(String::from)
                    .collect();
                let parent_coords = ctx
                    .current_parent_kernel
                    .as_ref()
                    .map(|k| {
                        k.scope_coordinates()
                            .iter()
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                // Algebra-native dispatch: one path for every
                // shape (Cartesian / Union / Zip / Filter /
                // Order). The runtime evaluator handles the
                // structure internally per spec §3.2. Order
                // applied to a Union samples across the union
                // as a whole (PR 9c-4 spec amendment).
                let coord_names = comprehension.coordinate_names();
                let needle = coord_names.first().cloned().unwrap_or_default();
                let steps = match runtime_iterate(
                    ctx,
                    &canonical,
                    &parent,
                    &parent_coords,
                    comprehension,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::phase_outcome::Outcome::failed()
                            .with_reason(enrich_with_yaml_location(ctx, &needle, e));
                    }
                };
                // Scene-tree path segment: ForEach for single
                // coord, ForCombinations for multi.
                let mut scope_path = ctx.scene_tree_path.clone();
                let (header, kind) = if coord_names.len() == 1 {
                    let var = &coord_names[0];
                    scope_path.push(PathSegment::ForEach { var: var.clone() });
                    (format!("each {var}"), "for_each")
                } else {
                    scope_path.push(PathSegment::ForCombinations {
                        vars: coord_names.clone(),
                    });
                    let summary = coord_names.join(", ");
                    (format!("each {summary}"), "for_combinations")
                };
                let scope_id = push_scope_scene_node(
                    ctx.scene_tree_parent_id,
                    scope_path.clone(),
                    header,
                    own_names.clone(),
                );
                // SRD-101 sweep gate (scenario `for:`) — compile against the
                // sweep's parent scope so coords + outer consts both resolve.
                let coord_sample = steps.first().map(|s| s.bindings.as_slice()).unwrap_or(&[]);
                let continue_if_gate = match resolve_continue_if(
                    continue_if.clone(),
                    &parent,
                    coord_sample,
                    ctx.strict,
                ) {
                    Ok(g) => g,
                    Err(e) => {
                        return crate::phase_outcome::Outcome::failed()
                            .with_reason(enrich_with_yaml_location(ctx, &needle, e));
                    }
                };
                let saved_parent = ctx.scene_tree_parent_id;
                let saved_path = std::mem::replace(&mut ctx.scene_tree_path, scope_path);
                let saved_scope_idx = ctx.current_scope_idx;
                ctx.scene_tree_parent_id = scope_id;
                ctx.current_scope_idx = scope_idx;
                let res = dispatch_comprehension(
                    ctx,
                    steps,
                    TerminalAction::Children(children),
                    depth + 1,
                    false,
                    kind,
                    None,
                    continue_if_gate,
                )
                .await;
                ctx.scene_tree_parent_id = saved_parent;
                ctx.scene_tree_path = saved_path;
                ctx.current_scope_idx = saved_scope_idx;
                return enrich_outcome(ctx, &needle, res); // SRD-92: propagate the real Outcome, yaml-enriched
            }
            ScenarioNode::IncludedScenario { name, children } => {
                // Structural push: scenario-include node. The
                // wrapper is "transparent" only in the sense that
                // it doesn't trigger execution — it still shows in
                // the scene tree so operators can trace the include
                // chain (SRD-44 §"Phase identity").
                if !ctx.quiet() {
                    crate::diag!(
                        crate::observer::LogLevel::Debug,
                        "include scenario '{name}' ({} children)",
                        children.len()
                    );
                }
                let mut scope_path = ctx.scene_tree_path.clone();
                scope_path.push(PathSegment::ScenarioInclude(name.clone()));
                let scope_id = push_scope_scene_node(
                    ctx.scene_tree_parent_id,
                    scope_path.clone(),
                    format!("scenario '{name}'"),
                    Vec::new(),
                );
                let saved_parent = ctx.scene_tree_parent_id;
                let saved_path = std::mem::replace(&mut ctx.scene_tree_path, scope_path);
                let saved_scope_idx = ctx.current_scope_idx;
                ctx.scene_tree_parent_id = scope_id;
                // One Walker positional cursor: descend with THIS node as the
                // scope parent so its children resolve against the right scopes.
                ctx.current_scope_idx = node_scope_idx;
                let res = execute_tree_at(ctx, children, depth + 1).await;
                ctx.scene_tree_parent_id = saved_parent;
                ctx.scene_tree_path = saved_path;
                ctx.current_scope_idx = saved_scope_idx;
                if res.is_failure() {
                    return res;
                } // SRD-92: propagate the subtree's real Outcome
            }
            ScenarioNode::DoWhile {
                condition,
                counter,
                children,
            } => {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "=== do_while: {condition} ==="
                );
                // Structural push: do_while scope header. Iteration
                // count is unknown a priori (condition-driven), so
                // there's no per-iter expansion at the scene tree
                // level — one scope node represents the whole loop.
                let mut scope_path = ctx.scene_tree_path.clone();
                scope_path.push(PathSegment::DoWhile {
                    counter: counter.clone(),
                });
                let own_names = do_loop_own_names(ctx, condition, counter.as_deref(), false);
                let scope_id = push_scope_scene_node(
                    ctx.scene_tree_parent_id,
                    scope_path.clone(),
                    format!("do_while {condition}"),
                    own_names,
                );
                let saved_parent = ctx.scene_tree_parent_id;
                let saved_path = std::mem::replace(&mut ctx.scene_tree_path, scope_path);
                let saved_scope_idx = ctx.current_scope_idx;
                ctx.scene_tree_parent_id = scope_id;
                ctx.current_scope_idx = node_scope_idx; // One Walker positional cursor
                fire_scope_lifecycle(
                    ctx,
                    crate::lifecycle::EventType::ScopeStart,
                    &format!("do_while {condition}"),
                    depth,
                );
                let r = run_do_loop(
                    ctx,
                    condition,
                    counter.as_deref(),
                    false,
                    children,
                    depth + 1,
                )
                .await;
                fire_scope_lifecycle(
                    ctx,
                    crate::lifecycle::EventType::ScopeEnd,
                    &format!("do_while {condition}"),
                    depth,
                );
                ctx.scene_tree_parent_id = saved_parent;
                ctx.scene_tree_path = saved_path;
                ctx.current_scope_idx = saved_scope_idx;
                if let Err(e) = r {
                    return crate::phase_outcome::Outcome::failed().with_reason(e);
                }
            }
            ScenarioNode::DoUntil {
                condition,
                counter,
                children,
            } => {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "=== do_until: {condition} ==="
                );
                let mut scope_path = ctx.scene_tree_path.clone();
                scope_path.push(PathSegment::DoUntil {
                    counter: counter.clone(),
                });
                let own_names = do_loop_own_names(ctx, condition, counter.as_deref(), true);
                let scope_id = push_scope_scene_node(
                    ctx.scene_tree_parent_id,
                    scope_path.clone(),
                    format!("do_until {condition}"),
                    own_names,
                );
                let saved_parent = ctx.scene_tree_parent_id;
                let saved_path = std::mem::replace(&mut ctx.scene_tree_path, scope_path);
                let saved_scope_idx = ctx.current_scope_idx;
                ctx.scene_tree_parent_id = scope_id;
                ctx.current_scope_idx = node_scope_idx; // One Walker positional cursor
                fire_scope_lifecycle(
                    ctx,
                    crate::lifecycle::EventType::ScopeStart,
                    &format!("do_until {condition}"),
                    depth,
                );
                let r = run_do_loop(
                    ctx,
                    condition,
                    counter.as_deref(),
                    true,
                    children,
                    depth + 1,
                )
                .await;
                fire_scope_lifecycle(
                    ctx,
                    crate::lifecycle::EventType::ScopeEnd,
                    &format!("do_until {condition}"),
                    depth,
                );
                ctx.scene_tree_parent_id = saved_parent;
                ctx.scene_tree_path = saved_path;
                ctx.current_scope_idx = saved_scope_idx;
                if let Err(e) = r {
                    return crate::phase_outcome::Outcome::failed().with_reason(e);
                }
            }
            ScenarioNode::Bindings { source, children } => {
                // Push the bindings scope's installed kernel as
                // ctx.current_parent_kernel so descendants'
                // workload-kernel rebuilds chain through this
                // scope's local matter. Lexical shadowing of an
                // upstream `final NAME` by this body's own
                // `const NAME := …` is enforced by the local-
                // final transit-suppression rule in
                // `materialize_wiring_from_outer` — uniform with
                // every other scope.
                //
                // The installed kernel was synthesised once at
                // workload-load time. When this Bindings scope
                // appears under a `for_each`, the enclosing
                // iteration's per-iter `bound_kernel` is what's
                // currently in `ctx.current_parent_kernel`; the
                // installed kernel's chain was wired against the
                // STRUCTURAL parent (no per-iter values). To pick
                // up iter-var bindings (and any per-iter shadows
                // from outer-scope `set:` nodes), build a fresh
                // subscope from the installed kernel's program
                // chained to the current parent. This is the
                // same chain-extension `dispatch_comprehension`
                // does for its per-iter bound_kernel
                // (`from_program → materialize_wiring_from_outer`
                // sequence, SRD-67 Phase 3).
                // Positional resolution (One Walker): same as the Comprehension
                // arm — the dispatcher mapped this node to its scope index by
                // position, so AST/source-identical sibling bindings resolve
                // correctly without the old content-keyed lookup.
                let scope_idx = node_scope_idx;
                debug_assert!(
                    matches!(
                        &ctx.scope_tree.nodes[scope_idx].kind,
                        crate::scope_tree::ScopeKind::Bindings { source: s } if s == source
                    ),
                    "bindings: positional scope node {scope_idx} is not the matching bindings",
                );
                let installed = match ctx.scope_tree.nodes[scope_idx].cached_kernel.get().cloned() {
                    Some(v) => v,
                    None => {
                        return crate::phase_outcome::Outcome::failed().with_reason(format!(
                            "bindings scope at index {scope_idx} has no installed \
                         kernel — install-spec walker missed this node",
                        ));
                    }
                };
                if !ctx.quiet() {
                    let one_line = source
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("");
                    crate::diag!(
                        crate::observer::LogLevel::Debug,
                        "bindings: {one_line} ({} children)",
                        children.len()
                    );
                }
                // Per-iter compile from the program preserves the
                // cached parse + wiring (same Arc<PolydatProgram>) while
                // giving us a fresh state that re-runs the const
                // materialisation (step 3 of
                // `materialize_wiring_from_outer`) against the
                // current parent's outputs. This is the same
                // recipe the for_each dispatcher uses for its
                // own per-iter `bound_kernel` (from_program →
                // materialize_wiring_from_outer). The cached
                // `installed` kernel's state held iter-1's
                // computed values; reusing it directly froze
                // every `const X := <expr-with-iter-var>` at the
                // first iter's value.
                let chained = match ctx.current_parent_kernel.as_ref() {
                    Some(parent) => {
                        let matter = match polydat::kernel::subcontext::PolydatMatter::builder()
                            .program(installed.program().clone())
                            .build()
                        {
                            Ok(v) => v,
                            Err(e) => {
                                return crate::phase_outcome::Outcome::failed().with_reason(
                                    format!(
                                        "bindings scope at index {scope_idx}: \
                                 build subscope matter: {e:?}",
                                    ),
                                );
                            }
                        };
                        match parent.build_subscope(matter).map(std::sync::Arc::new) {
                            Ok(v) => v,
                            Err(e) => {
                                return crate::phase_outcome::Outcome::failed().with_reason(
                                    format!(
                                        "bindings scope at index {scope_idx}: \
                                 chain to current parent kernel: {e:?}",
                                    ),
                                );
                            }
                        }
                    }
                    None => installed,
                };
                // Structural push: bindings scope header. SRD 18b
                // §"Single Walker Contract" point 2 — the bindings
                // arm pushes the scene-tree node uniformly with
                // every other arm. The original asymmetry bug had
                // this arm "transparent" in pre-map while the
                // runtime did build_subscope; the unified walker
                // does both at every depth.
                // Label = the names this scope defines (comment-only
                // first lines made the raw-source label read as a
                // repeating stray comment in the readout).
                let label = crate::scope_tree::bindings_label(source);
                let mut scope_path = ctx.scene_tree_path.clone();
                scope_path.push(PathSegment::ScenarioInclude(label.clone()));
                let scope_id = push_scope_scene_node(
                    ctx.scene_tree_parent_id,
                    scope_path.clone(),
                    label,
                    Vec::new(),
                );
                let prior_parent = ctx.current_parent_kernel.take();
                ctx.current_parent_kernel = Some(chained);
                let saved_scene_parent = ctx.scene_tree_parent_id;
                let saved_scene_path = std::mem::replace(&mut ctx.scene_tree_path, scope_path);
                let saved_scope_idx = ctx.current_scope_idx;
                ctx.scene_tree_parent_id = scope_id;
                ctx.current_scope_idx = scope_idx;
                let res = execute_tree_at(ctx, children, depth + 1).await;
                ctx.scene_tree_parent_id = saved_scene_parent;
                ctx.scene_tree_path = saved_scene_path;
                ctx.current_scope_idx = saved_scope_idx;
                ctx.current_parent_kernel = prior_parent;
                if res.is_failure() {
                    return res;
                } // SRD-92: propagate the subtree's real Outcome
            }
        }
        crate::phase_outcome::Outcome::completed()
    })
}

// =====================================================================
// Unified comprehension dispatcher — SRD 18b §"M3 — per-scope
// kernel composition". One control-loop harness for every
// iteration kind: for_each / for_combinations / for_each_union
// (tuple comprehensions), do_while / do_until (counter-driven
// loops), and phase-level for_each. Each iteration kind plugs in
// via the [`Comprehension`] strategy trait, which produces
// successive iteration bindings; the dispatcher's single
// per-branch loop applies those bindings to a fresh per-branch
// kernel (`PolydatKernel::from_program` from the scope's installed
// canonical) and runs the children under it. No duplicated
// recursion logic per iteration kind.
// =====================================================================

/// Strategy plugin: produce the next iteration's typed bindings.
/// `Ok(Some(_))` = run children with these bindings; `Ok(None)` =
/// halt; `Err(_)` = propagate up.
///
/// Activity-side adapter over the GK
/// [`polydat::iteration::comprehension::iterate_scope`] driver:
/// applies strict-vs-warn empty-clause policy with diag emission
/// honoring `ExecCtx::quiet()`. The runtime executor and the
/// pre-map walker both go through `iterate_scope`; `runtime_iterate`
/// just adds the activity-layer concerns (warn-level logging) to
/// each iteration request.
///
/// Returns the materialised step list — runtime needs to know the
/// count up front to decide serial vs. concurrent dispatch
/// One iteration position of a comprehension scope, ready for
/// downstream consumption by the dispatch loop. Local to the
/// executor since 9c-4b — was previously in
/// `polydat::iteration::comprehension::iteration` but is purely an
/// executor-side per-iteration record.
#[derive(Clone, Debug)]
pub struct IterationStep {
    /// Typed `(var, value)` pairs for this iteration.
    pub bindings: Vec<(String, polydat::ast::Value)>,
    /// Per-iteration kernel: clone of the comprehension's
    /// canonical, bound to the parent scope, with every input
    /// in [`Self::bindings`] populated. Descendants treat
    /// this as their effective parent kernel — both for
    /// nested comprehension interpolation (`vec_{profile}`)
    /// and for runtime phase dispatch.
    pub bound_kernel: std::sync::Arc<polydat::kernel::PolydatKernel>,
    /// Root-first scope-coordinate chain ending at this
    /// iteration. Pass through
    /// `polydat::kernel::format_scope_coordinate_path` (after
    /// reversing to leaf-first) to get the canonical
    /// structural label string.
    pub coord_path: Vec<polydat::kernel::ScopeCoord>,
}

/// (`schedule=` limits, single-iter fast path).
/// Drive the algebra runtime evaluator and materialise the
/// resulting tuples into [`IterationStep`]s the dispatcher
/// consumes. Single entry point for every comprehension shape
/// — Cartesian / Union / Zip / Filter / Order all funnel
/// through the algebra-native evaluation per spec §3.2's
/// dependent-product semantics.
///
/// Per spec amendment (PR 9c-4): Order applied to a Union
/// samples across the union as a whole (not per sub-space) —
/// matches the algebra's `Order(Union(...))` structural
/// reading.
fn runtime_iterate(
    ctx: &ExecCtx,
    canonical: &std::sync::Arc<polydat::kernel::PolydatKernel>,
    parent: &std::sync::Arc<polydat::kernel::PolydatKernel>,
    parent_coords: &[ScopeCoord],
    comprehension: &polydat::iteration::comprehension::Comprehension,
) -> Result<Vec<IterationStep>, String> {
    use polydat::iteration::comprehension::runtime::{EmptyClause, evaluate_for_iteration};
    use polydat::kernel::{PolydatKernel, ScopeCoord};

    let strict = ctx.strict;
    let quiet = ctx.quiet();
    let on_empty = |empty: EmptyClause<'_>| -> Result<(), String> {
        let label = match empty.spec_expr {
            Some(spec) => format!("for_each clause '{var} in {spec}'", var = empty.var),
            None => format!("for_each clause '{var}'", var = empty.var),
        };
        let msg = format!("{label}: produced no values");
        if strict {
            return Err(format!("strict: {msg}"));
        }
        if !quiet {
            crate::diag!(crate::observer::LogLevel::Warn, "warning: {msg}");
        }
        Ok(())
    };

    // polydat 0.3: the evaluator reads names through `Lookup` — the
    // parent kernel is the scope; the canonical program is only
    // needed below, to materialise each tuple's per-iteration kernel.
    let tuples = evaluate_for_iteration(
        comprehension,
        parent.as_ref(),
        &ctx.workload_params,
        on_empty,
    )
    .map_err(|e| e.to_string())?;

    // Materialise each tuple into an IterationStep: per-iter
    // kernel via PolydatKernel::for_iteration, coord path extended
    // from parent_coords. The runtime evaluator already gives
    // us polydat-Value tuples (RuntimeTuple), so no conversion
    // is needed — Ext-typed Partition values pass through
    // intact for the executor's Ext-slot binding.
    let mut steps = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        let bound_kernel = PolydatKernel::for_iteration(canonical, parent, &tuple);
        let mut coord_path = parent_coords.to_vec();
        coord_path.push(ScopeCoord::from(tuple.iter().cloned()));
        steps.push(IterationStep {
            bindings: tuple,
            bound_kernel,
            coord_path,
        });
    }
    Ok(steps)
}

// `Comprehension` trait + `TupleComprehension` retired: the
// dependent-tuple walk + per-iteration kernel binding is now
// owned by `polydat::iteration::comprehension::iterate_scope` and the
// types it returns. Both runtime (`runtime_iterate`) and pre-map
// (`premap_iterate`) call into the same Polydat primitive.
//
// Do-loops (`do_while` / `do_until`) bypass this path — they
// need a persistent kernel across iterations (counter
// `set_input`, condition eval, interleaved with child
// execution) which doesn't fit the eager-enumeration shape.
// See [`run_do_loop`] for the streaming dispatcher.

/// What runs at the leaf of each comprehension iteration. The
/// dispatcher switches on this for the per-iteration terminal
/// step — scenario-level for/do scopes descend into children;
/// phase-level for_each runs the phase itself once per
/// iteration value.
#[derive(Clone)]
pub enum TerminalAction<'a> {
    Children(&'a [ScenarioNode]),
    Phase(&'a str),
}

/// Owned form of [`TerminalAction`] for moving into spawned
/// concurrent tasks. `borrow()` reconstructs a borrowed
/// `TerminalAction` for the dispatcher's per-iteration call.
#[derive(Clone)]
enum OwnedTerminal {
    Children(std::sync::Arc<Vec<ScenarioNode>>),
    Phase(String),
}

impl OwnedTerminal {
    fn borrow(&self) -> TerminalAction<'_> {
        match self {
            OwnedTerminal::Children(arc) => TerminalAction::Children(arc.as_slice()),
            OwnedTerminal::Phase(name) => TerminalAction::Phase(name.as_str()),
        }
    }
}

/// SRD-101 — a `continue_if` sweep gate resolved for dispatch: the parsed spec
/// (predicate text + `each` halt scope), the compiled gate kernel, and the
/// sweep's parent scope. Per iteration, [`crate::stop_conditions::eval_continue_if`]
/// materialises the gate kernel under `parent` (consts cascade in) bound to the
/// iteration coordinates, and pulls the predicate — see [`resolve_continue_if`].
struct ContinueIfGate {
    spec: nmbrs_workload::model::ContinueIfSpec,
    gate_canonical: std::sync::Arc<polydat::kernel::PolydatKernel>,
    parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
}

/// SRD-101 — resolve a `continue_if` spec for a sweep: compile its predicate
/// into a gate kernel (coordinates pre-typed from `coord_sample`, outer consts
/// auto-externed) and capture the sweep's `parent` scope for per-iteration
/// materialisation. `coord_sample` is one representative iteration's bindings
/// (coordinate names + runtime types). `Ok(None)` when no gate is declared;
/// `Err` surfaces a predicate compile error.
fn resolve_continue_if(
    spec: Option<nmbrs_workload::model::ContinueIfSpec>,
    parent: &std::sync::Arc<polydat::kernel::PolydatKernel>,
    coord_sample: &[(String, polydat::ast::Value)],
    strict: bool,
) -> Result<Option<ContinueIfGate>, String> {
    match spec {
        None => Ok(None),
        Some(spec) => {
            let gate_canonical =
                crate::stop_conditions::compile_continue_if(&spec.when, coord_sample, strict)?;
            Ok(Some(ContinueIfGate {
                spec,
                gate_canonical,
                parent: parent.clone(),
            }))
        }
    }
}

/// Unified comprehension dispatcher. Drains the strategy into a
/// flat tuple list, then walks it through the semaphore-gated
/// `JoinSet` harness per the level's `schedule=` policy. Per SRD
/// 02 §"One Concurrency Path": one path, parameterised by
/// `concurrency_limit`. `Bounded(1)` is the sequential case (one
/// permit at a time → spawn order = drain order). Each iteration:
///
/// 1. Builds a fresh per-branch `PolydatKernel` via `from_program`.
/// 2. `materialize_wiring_from_outer(parent_kernel)` for inheritance.
/// 3. `set_input` for each iteration-variable value.
/// 4. Pushes itself as `ctx.current_parent_kernel` so leaf
///    phases (and any nested comprehensions) inherit through
///    standard Polydat chain.
/// 5. Pushes labels for the iteration's variables.
/// 6. Runs the [`TerminalAction`] — either descend into
///    children via `execute_tree_at` or `run_phase` for
///    phase-level `for_each`.
/// 7. Pops labels, restores `current_parent_kernel`.
///
/// `sequential_only` forces the per-iteration limit to
/// `Bounded(1)` regardless of `schedule=` — used for do-loops
/// where iteration N depends on iteration N-1's effects (would
/// need to be revisited for `shared`-state propagation; SRD-16
/// §"Shared Mutable"). The harness is unchanged; only the limit
/// value differs.
fn dispatch_comprehension<'a>(
    ctx: &'a mut ExecCtx,
    steps: Vec<IterationStep>,
    terminal: TerminalAction<'a>,
    depth: usize,
    sequential_only: bool,
    kind: &'static str,
    // Phase-terminal scene-tree metadata. `Some((name,
    // op_names, phase_yaml_path))` for `TerminalAction::Phase`
    // — the dispatcher pushes one Phase scene node per
    // iteration, labels = canonical_phase_label(step.coord_path),
    // so run_phase's later `set_phase_running` find_phase
    // lookup matches. `None` for `TerminalAction::Children`
    // — per-iter inner scope is derived from the step
    // bindings + outer `ctx.scene_tree_path`.
    phase_terminal_meta: Option<(String, Vec<String>, Vec<crate::checkpoint::PathSegment>)>,
    // SRD-101 — optional `continue_if` pre-entry gate bounding this sweep
    // (the parsed spec + compiled gate kernel + the sweep's parent scope).
    continue_if: Option<ContinueIfGate>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>>
{
    use crate::scheduler::ConcurrencyLimit;
    Box::pin(async move {
        if steps.is_empty() {
            // Polydat-side `iterate_scope` already routed any clause-
            // level diagnostic through the strict-vs-warn callback;
            // an empty step list here is the success-with-zero-
            // iterations case (filter eliminated everything, or a
            // do-loop condition false from the start).
            return crate::phase_outcome::Outcome::completed();
        }

        // Effective limit: `sequential_only` (do-loop ordering
        // dependency) clamps to Bounded(1); otherwise read the
        // schedule spec. There is no separate serial code path —
        // Bounded(1) flows through the same semaphore-gated
        // dispatcher as any other limit.
        let limit = if sequential_only {
            ConcurrencyLimit::Bounded(1)
        } else {
            ctx.schedule_spec.limit_at(depth)
        };

        let sem: Option<std::sync::Arc<tokio::sync::Semaphore>> = match limit {
            ConcurrencyLimit::Bounded(n) => {
                Some(std::sync::Arc::new(tokio::sync::Semaphore::new(n as usize)))
            }
            ConcurrencyLimit::Unlimited => None,
        };
        // The TerminalAction borrows from the caller's slice;
        // for spawning into 'static futures, materialize into an
        // owned form. Children slice → owned Vec; Phase name →
        // owned String.
        let owned_terminal = match &terminal {
            TerminalAction::Children(c) => OwnedTerminal::Children(std::sync::Arc::new(c.to_vec())),
            TerminalAction::Phase(name) => OwnedTerminal::Phase(name.to_string()),
        };

        // Inner-scope per-iter pushes need own_names from the
        // outer comprehension's installed kernel. Caller has
        // pushed the outer scope and updated ctx.scene_tree_*
        // before invoking us; we read own_names from the outer
        // scope's node so per-iter inner scopes inherit the same
        // (matches pre_map's set_own_names(inner_scope, own.clone())).
        let inner_own_names: Vec<String> = if phase_terminal_meta.is_none() {
            crate::scene_tree::current()
                .and_then(|t| {
                    t.nodes
                        .get(ctx.scene_tree_parent_id)
                        .map(|n| n.own_names.clone())
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Permit acquired in the dispatcher loop, not the
        // spawned task — see the matching comment in
        // `run_siblings_concurrently`. Bounded(N) caps in-flight
        // count and blocks the (N+1)th iteration's dispatch
        // until a permit frees up; Unlimited spawns all in
        // declaration order with no waiting. Either way the
        // order in which iterations are LOFTED for execution
        // is the deterministic comprehension-step order.
        let mut set = tokio::task::JoinSet::new();
        // SRD-92 Step 5d — drive the comprehension iterations through the
        // unified `ChildSource` contract: a realized instance list → `Realizable`
        // → `BoundedSpawn`. The `CountedSource` gates the per-iteration drive
        // (one `poll_next` per instance, in declaration order); the owned
        // `IterationStep`s are consumed in lockstep (no extra clone — identical
        // to the old `for step in steps`).
        use crate::child_source::{Child, ChildSource, CountedSource, Drive, select_drive};
        let mut csrc = CountedSource::new(steps.len());
        debug_assert_eq!(select_drive(csrc.realizability()), Drive::BoundedSpawn);
        let mut steps_iter = steps.into_iter();
        // SRD-101 — set when a `continue_if` gate goes false: the sweep stops
        // continuing and reports a graceful Interrupted+Succeeded outcome
        // carrying this reason (the `PhaseOutcome` marker, §7).
        let mut continue_if_halt: Option<String> = None;
        while let Some(Child::Node(_)) = csrc.poll_next() {
            let step = steps_iter
                .next()
                .expect("CountedSource length matches steps");
            // SRD-101 — `continue_if` PRE-ENTRY gate. Evaluate the predicate
            // against THIS iteration's coordinate context (its bound kernel)
            // BEFORE entering the body. While true the iteration runs; the
            // moment it is false, halt the sweep gracefully (no body, the
            // break ends dispatch; in-flight iterations drain at the join).
            // Executional only — gated on depth >= Op AND not the structural
            // pre-map pass (SRD-18b One Walker: structural always, executional
            // gated). The pre-map walker maps op nodes at depth >= Op but binds
            // placeholder coordinate values (e.g. a `u64` stand-in for an
            // `Ext<Partition>`), so evaluating the gate there would mis-type;
            // the cap is a real-execution decision, and pre-map shows the full
            // structural tree.
            if let Some(gate) = continue_if.as_ref() {
                if ctx.diag.depth >= crate::runner::ExecDepth::Op && !ctx.pre_map_only {
                    match crate::stop_conditions::eval_continue_if(
                        &gate.gate_canonical,
                        &gate.parent,
                        &step.bindings,
                    ) {
                        Ok(true) => {} // gate holds → run this iteration
                        Ok(false) => {
                            let coord = format_iter_label(&step.bindings);
                            let reason =
                                format!("continue_if: {} — halted at {coord}", gate.spec.when);
                            crate::diag!(
                                crate::observer::LogLevel::Info,
                                "sweep halted (continue_if): `{}` false at {coord}",
                                gate.spec.when
                            );
                            // Signal a graceful early stop so the post-run
                            // "pre-mapped phase(s) not executed" guard treats
                            // the halted tail as deliberate (clean exit 0),
                            // exactly as an SRD-83 `stop` effect does. This is
                            // a post-run flag only — it does NOT itself halt
                            // the walk (the break / walk_stop latch does).
                            crate::session_signals::request_graceful_stop();
                            // `each: workload` propagates the halt to the whole
                            // run via the shared walk_stop latch; otherwise the
                            // local break is the halt and the walk above this
                            // sweep continues.
                            if gate
                                .spec
                                .each
                                .iter()
                                .any(|l| matches!(l, nmbrs_workload::model::ScopeLevel::Workload))
                            {
                                ctx.workload_shell.request_stop(
                                    crate::phase_outcome::Outcome::interrupted()
                                        .with_reason(reason.clone()),
                                    reason.clone(),
                                );
                            }
                            continue_if_halt = Some(reason);
                            break;
                        }
                        Err(e) => {
                            return crate::phase_outcome::Outcome::failed().with_reason(e);
                        }
                    }
                }
            }
            let permit = match sem.as_ref() {
                Some(s) => match s.clone().acquire_owned().await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        return crate::phase_outcome::Outcome::failed().with_reason(e.to_string());
                    }
                },
                None => None,
            };
            // SRD-83 — stop iterating once the walk has halted (a prior
            // iteration's phase failed, a stop condition tripped, or a
            // sibling subtree faulted). At Bounded(1) the permit acquire
            // above already waited for the previous iteration to finish,
            // so its `walk_stop` latch is visible here. Each remaining
            // step would early-return in `run_scenario_body` anyway, but
            // breaking ends the sweep immediately instead of spinning
            // through every remaining iteration (its per-iter scene-node
            // push + task spawn). Already-in-flight iterations (Bounded
            // N>1) drain at the join below.
            if ctx.workload_shell.should_stop() {
                drop(permit);
                break;
            }
            // Structural push: per-iter scene-tree node. Pushed
            // in the dispatcher loop (not the spawned task) so
            // order is deterministic and stable phase seq numbers
            // get assigned in DFS / iteration order.
            let per_iter_scene_id = match &phase_terminal_meta {
                Some((phase_name, op_names, phase_yaml_path)) => {
                    // SRD-100 — wrap each cell in its OWN per-iter scope
                    // (keyed by the iteration binding), then push the phase
                    // under it. Same-name cells become distinct nodes via
                    // distinct PARENTS, never aliased by `push`'s
                    // name-idempotency — so a concurrent sweep's lifecycle
                    // routing (P1c) and per-cell display attribute correctly.
                    // The phase node's identity stays = name (no byte-exact
                    // coordinate-label-string coupling, §4 invariant); the
                    // distinctness is carried by the scope segment, which is
                    // where an iteration binding algebraically belongs — and
                    // it makes phase-level `for_each` topology IDENTICAL to
                    // the scenario-level (Children) form below. The phase
                    // (child of the per-iter scope) is the leaf threaded
                    // into `run_phase`; its `yaml_path` is unchanged, so
                    // checkpoint identity `(yaml_path, coords)` is preserved.
                    let iter_scope = push_scope_scene_node(
                        ctx.scene_tree_parent_id,
                        ctx.scene_tree_path.clone(),
                        format_iter_label(&step.bindings),
                        Vec::new(),
                    );
                    let labels = canonical_phase_label(&step.coord_path);
                    push_phase_scene_node(
                        iter_scope,
                        phase_yaml_path.clone(),
                        phase_name.clone(),
                        labels,
                        op_names.clone(),
                    )
                }
                None => push_scope_scene_node(
                    ctx.scene_tree_parent_id,
                    ctx.scene_tree_path.clone(),
                    format_iter_label(&step.bindings),
                    inner_own_names.clone(),
                ),
            };
            let mut task_ctx = ctx.clone();
            // For Children terminal: per-iter inner scope becomes
            // the parent for descended children. For Phase
            // terminal: the per-iter Phase node IS the leaf — no
            // descent — but set parent anyway for consistency.
            task_ctx.scene_tree_parent_id = per_iter_scene_id;
            let owned_terminal = owned_terminal.clone();
            // SRD-88: propagate the per-execution context into the spawned
            // comprehension-iteration fiber (A1 no-op for single-run).
            set.spawn(crate::execution_context::propagate(async move {
                let _permit = permit;
                let terminal = owned_terminal.borrow();
                run_one_iteration(&mut task_ctx, &step, &terminal, depth, kind).await
            }));
        }

        let mut first_err: Option<String> = None;
        while let Some(res) = set.join_next().await {
            // SRD-92: each iteration task now yields its real Outcome.
            match res {
                Err(join_err) => {
                    if first_err.is_none() {
                        first_err = Some(format!(
                            "concurrent comprehension iteration panicked: {join_err}"
                        ));
                    }
                }
                Ok(o) => {
                    if o.is_failure() && first_err.is_none() {
                        first_err = Some(
                            o.reason
                                .unwrap_or_else(|| "comprehension iteration failed".to_string()),
                        );
                    }
                }
            }
        }
        // SRD-101 — a `continue_if` gate halted the sweep: report it as a
        // graceful Interrupted+Succeeded outcome carrying the marker reason
        // (the sweep scope's `PhaseOutcome`, §7), UNLESS an in-flight iteration
        // failed meanwhile — a real failure wins (honest).
        if let Some(reason) = continue_if_halt {
            if first_err.is_none() {
                return crate::phase_outcome::Outcome::interrupted().with_reason(reason);
            }
        }
        // SRD-92 two-latch fold. `first_err` is the validity latch (already set
        // on ANY failed/panicked iteration — comprehension drains all, no local
        // cascade), so the fix is the DISPOSITION: an all-ran comprehension with
        // a failed iteration is Completed+Failed, not Interrupted+Failed; only a
        // workload-shell halt cuts it short. (`to_status()` is Failed either way.)
        fold_aggregate(None, first_err, ctx.workload_shell.should_stop())
    })
}

/// SRD-86 A12 — drive an `optimize:` node as a **search**, not a sweep. The
/// optimizer (resolved from the link-time registry) produces a pull-through
/// coordinate stream; we run the phase once per pulled coordinate **sequentially**
/// (the next coordinate depends on the last objective), read the objective
/// metric off the iteration's kernel, and feed it back. The candidate grid in
/// `steps` only derives the discrete `SearchSpace` — the node represents a
/// bounded search, and pre-map (depth < Op) pre-maps the coordinate-invariant
/// subtree once.
fn dispatch_optimization<'a>(
    ctx: &'a mut ExecCtx,
    space: crate::optimize::SearchSpace,
    coord_eval: CoordEval,
    block: nmbrs_workload::model::OptimizeBlock,
    phase_name: &'a str,
    depth: usize,
    phase_meta: Option<(String, Vec<String>, Vec<crate::checkpoint::PathSegment>)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>>
{
    Box::pin(async move {
        // SRD-92 — the niche `optimize:` subtree stays Result internally
        // (its helpers + run_one_eval); wrap to the universal Outcome here.
        let __opt_result: Result<(), String> = async move {
        if space.dims() == 0 {
            return Ok(());
        }

        // Pre-map (depth < Op): represent the search + pre-map the
        // coordinate-invariant subtree ONCE (one representative evaluation —
        // the first enumerated step, or the continuous space's centre).
        // `run_one_iteration`'s Phase terminal is itself depth-gated, so this is
        // a purely structural pass — no real phase run, no objective read.
        if ctx.diag.depth < crate::runner::ExecDepth::Op {
            if let Some(step) = coord_eval.representative(&space) {
                return run_one_eval(ctx, &step, phase_name, depth, &phase_meta).await;
            }
            return Ok(());
        }

        // SRD-86 §4 Control-class actuation. An axis declared `changeover:
        // control` is SERVOED (live retarget); the rest are coordinate axes
        // (rerun). Three shapes:
        //  - all-control  → ONE continuous phase, daemon servos everything.
        //  - mixed        → hybrid: iterate the coordinate axes (rerun) and servo
        //                   the control axes interior to each cell.
        //  - none         → fall through to the Coordinate adaptive loop below.
        let phase_concurrency = ctx.phases.get(phase_name).and_then(|p| p.concurrency.clone());
        let phase_has_rate = ctx.phases.get(phase_name).is_some_and(|p| p.rate.is_some());
        let control_axes = classify_control_axes(
            &space,
            &block.servo,
            phase_concurrency.as_deref(),
            phase_has_rate,
        )?;
        if !control_axes.is_empty() {
            if control_axes.len() == space.dims() {
                return run_control_search(
                    ctx, space, coord_eval, control_axes, block, phase_name, depth, phase_meta,
                )
                .await;
            }
            return run_hybrid_search(
                ctx, space, coord_eval, control_axes, block, phase_name, depth, phase_meta,
            )
            .await;
        }

        // ─── Execution: the adaptive search loop (Coordinate actuation) ───
        let mut params = crate::optimize::OptimizerParams::new();
        for (k, v) in &block.params {
            params = params.with(k.clone(), *v);
        }
        let optimizer = crate::optimize::by_name(&block.method, &params).ok_or_else(|| {
            format!("phase '{phase_name}': unknown optimizer method '{}'", block.method)
        })?;
        let budget = crate::optimize::Budget::seeded(block.max_evals, block.seed);
        let lex: Box<dyn crate::optimize::PullSource> =
            Box::new(crate::optimize::LexSource::new(&space));
        let mut src = optimizer.coordinate_source(&space, &budget, lex);

        // Ask `run_phase` to read this objective wire off each iteration's single
        // live kernel (SRD-86 — a fully-qualified-on-node objective). An inline
        // objective expression resolves to the synthesized `__objective` wire;
        // a bare name is read directly (`crate::scope::objective_wire`).
        ctx.optimize_objective = Some(crate::scope::objective_wire(&block.objective).to_string());

        let mut best_value = f64::NEG_INFINITY;
        let mut best_coord: Option<crate::optimize::Coord> = None;
        let mut evals = 0usize;

        let mut batch = source_next(&mut src, &[]);
        'outer: while let Some(coords) = batch.take() {
            let mut evaluated: Vec<(crate::optimize::Coord, f64)> = Vec::new();
            for coord in coords {
                if evals >= block.max_evals {
                    break 'outer;
                }
                let Some(step) = coord_eval.materialize(&coord) else {
                    crate::diag!(crate::observer::LogLevel::Warn,
                        "optimizer '{method}' on '{phase_name}': coordinate [{key}] is not in the \
                         enumerable grid — skipping",
                        method = block.method, key = coord_key(&coord));
                    continue;
                };
                run_one_eval(ctx, &step, phase_name, depth, &phase_meta).await?;
                evals += 1;
                let value = ctx.optimize_objective_value.ok_or_else(|| {
                    format!(
                        "phase '{phase_name}': optimize objective '{}' produced no value — it must \
                         be a numeric wire fully qualified on the phase node",
                        block.objective
                    )
                })?;
                if value > best_value {
                    best_value = value;
                    best_coord = Some(coord.clone());
                }
                evaluated.push((coord, value));
            }
            batch = source_next(&mut src, &evaluated);
        }
        ctx.optimize_objective = None;

        let best_disp = best_coord
            .as_ref()
            .map(|c| c.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "<none>".into());
        crate::diag!(crate::observer::LogLevel::Info,
            "optimizer '{method}' on '{phase_name}': best [{best_disp}] → {objective}={best_value} \
             after {evals} evals",
            method = block.method, objective = block.objective);
        Ok(())
        }.await;
        match __opt_result {
            Ok(()) => crate::phase_outcome::Outcome::completed(),
            Err(reason) => crate::phase_outcome::Outcome::failed().with_reason(reason),
        }
    })
}

/// SRD-86 §4 — resolve the **explicitly servoed** axes (`optimize.servo`). Every
/// axis is a **coordinate** (stepped through by re-running the phase) by default;
/// a var named in `servo` is actuated as a live **control** instead. Each servoed
/// var is *validated*: it must be a search axis, and it resolves to a control
/// either **directly** (its name IS a live control — `servo: concurrency`,
/// `servo: rate`) or **indirectly** (it sinks into a control via a `{var}`-bind,
/// `concurrency: "{conc}"` → the `concurrency` control). Neither → a clear error,
/// not a silent downgrade. (The other half of "servoing is meaningful" — that the
/// objective is a windowed metric the servo can settle — is checked downstream
/// by [`require_windowed_objective`].) Returns the control axes (`axis_idx` into
/// `space`); the rest are coordinate axes — a node may mix the two (the hybrid
/// path). The direct form is the only way to servo `rate`, whose `f64` field
/// can't carry a `{var}`.
fn classify_control_axes(
    space: &crate::optimize::SearchSpace,
    servo: &[String],
    phase_concurrency: Option<&str>,
    phase_has_rate: bool,
) -> Result<Vec<crate::optimize::servo::ControlAxis>, String> {
    use crate::optimize::servo::ControlAxis;
    // The live controls a phase declares (SRD-23). A servoed var resolves to one
    // either DIRECTLY (its name IS a control — `servo: concurrency`) or
    // INDIRECTLY (it feeds a control via `concurrency: "{var}"` — `servo: conc`).
    const KNOWN_CONTROLS: &[&str] = &["concurrency", "rate"];
    let mut controls = Vec::new();
    for var in servo {
        // The servoed var must be a search axis (gathered from the `for_each`).
        let Some(i) = space.axes.iter().position(|ax| &ax.name == var) else {
            return Err(format!(
                "optimize `servo: {var}` is not a search axis — name a var that appears in the \
                 phase's `for_each`"
            ));
        };
        let control = if KNOWN_CONTROLS.contains(&var.as_str()) {
            // Direct: the axis var IS a live control — servo it by name, no
            // `{var}`-bind wire needed. (This is how `rate` is servo at all,
            // since `rate:` can't carry a `{var}`.) The `concurrency` control is
            // always declared; the `rate` control only when the phase sets `rate:`,
            // so servoing `rate` requires that field — caught here, not at runtime.
            if var == "rate" && !phase_has_rate {
                return Err(
                    "optimize `servo: rate` but the phase declares no `rate:` field, so there is no \
                     rate control to servo — add a `rate:` to the phase (its value is the warmup the \
                     servo retargets from)"
                        .to_string(),
                );
            }
            var.clone()
        } else if phase_concurrency.is_some_and(|c| c.contains(&format!("{{{var}}}"))) {
            // Indirect: the var sinks into the `concurrency` control via
            // `concurrency: "{var}"`. (Textual match for now; the principled form
            // traces the var's l-value flow to a control sink.)
            "concurrency".to_string()
        } else {
            return Err(format!(
                "optimize `servo: {var}` but '{var}' is neither a live control nor wired to one — \
                 name a control directly (`servo: concurrency`) or wire the var \
                 (`concurrency: \"{{{var}}}\"`); or drop it from `servo:` to step through its \
                 values by re-running the phase"
            ));
        };
        controls.push(ControlAxis {
            axis_idx: i,
            control,
        });
    }
    Ok(controls)
}

/// A Control-class sweep settles a windowed objective per setting — reject an
/// objective that reads no live metric upfront, with an actionable message.
fn require_windowed_objective(
    ctx: &ExecCtx,
    phase_name: &str,
    objective: &str,
) -> Result<(), String> {
    let phase_kernel = ctx
        .scope_tree
        .phase_node_by_name(phase_name)
        .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get().cloned());
    if let Some(pk) = &phase_kernel
        && !crate::optimize::settle::program_reads_live_metrics(pk.program())
    {
        return Err(format!(
            "phase '{phase_name}': Control-class optimize needs a windowed objective \
             ('{objective}') to settle per setting, but the phase reads no live metric. \
             Use metric_window(...) or metricsql_scalar(rate(...[W]))."
        ));
    }
    Ok(())
}

/// Run the Control daemon for ONE phase activation (one continuous phase bound at
/// `step`): set up the [`ServoSpec`](crate::optimize::servo::ServoSpec) over
/// `space`/`controls`, run the phase with the daemon `tokio::join!`'d in
/// `run_phase`, and return what it found. Shared by the pure-control path (one
/// cell at the centre) and the hybrid path (one cell per coordinate combination).
#[allow(clippy::too_many_arguments)] // mirrors `dispatch_optimization`'s shape
async fn run_servo_cell(
    ctx: &mut ExecCtx,
    step: &IterationStep,
    space: crate::optimize::SearchSpace,
    controls: Vec<crate::optimize::servo::ControlAxis>,
    block: &nmbrs_workload::model::OptimizeBlock,
    phase_name: &str,
    depth: usize,
    phase_meta: &Option<(String, Vec<String>, Vec<crate::checkpoint::PathSegment>)>,
) -> Result<crate::optimize::servo::ServoOutcome, String> {
    let result = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::optimize::servo::ServoOutcome::default(),
    ));
    let spec = crate::optimize::servo::ServoSpec {
        method: block.method.clone(),
        params: block.params.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        // Inline objective expr → synthesized `__objective` wire; bare name read
        // directly. The servo daemon settles this wire off the phase kernel.
        objective: crate::scope::objective_wire(&block.objective).to_string(),
        max_evals: block.max_evals,
        seed: block.seed,
        space,
        controls,
        result: result.clone(),
    };
    // The servo owns settling; leave `optimize_objective` None so `run_phase`'s
    // own per-phase settle does NOT fire.
    ctx.optimize_objective = None;
    ctx.optimize_servo = Some(spec);
    run_one_eval(ctx, step, phase_name, depth, phase_meta).await?;
    ctx.optimize_servo = None;
    Ok((**result.load()).clone())
}

/// SRD-86 §4–§6 — drive an **all-control** search: run ONE continuous phase and
/// `tokio::join!` the [`servo`](crate::optimize::servo::servo) daemon to
/// live-retarget its controls per setting. The phase starts at the space centre
/// (a neutral warmup binding); the servo retargets from there.
#[allow(clippy::too_many_arguments)] // mirrors `dispatch_optimization`'s shape
fn run_control_search<'a>(
    ctx: &'a mut ExecCtx,
    mut space: crate::optimize::SearchSpace,
    coord_eval: CoordEval,
    control_axes: Vec<crate::optimize::servo::ControlAxis>,
    block: nmbrs_workload::model::OptimizeBlock,
    phase_name: &'a str,
    depth: usize,
    phase_meta: Option<(String, Vec<String>, Vec<crate::checkpoint::PathSegment>)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        require_windowed_objective(ctx, phase_name, &block.objective)?;
        // Stamp Control on the servoed axes so any describe/dryrun renders truthfully.
        for ca in &control_axes {
            space.axes[ca.axis_idx].changeover = crate::optimize::Changeover::Control;
        }
        // The continuous phase starts at the space centre; the servo retargets
        // each control immediately, so this is only the warmup setting.
        let center = coord_eval.representative(&space).ok_or_else(|| {
            format!("phase '{phase_name}': control optimize has no representative coordinate")
        })?;
        let outcome = run_servo_cell(
            ctx,
            &center,
            space,
            control_axes,
            &block,
            phase_name,
            depth,
            &phase_meta,
        )
        .await?;
        let (best_disp, best_value) = match &outcome.best {
            Some(b) => (
                b.coord
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                b.value,
            ),
            None => ("<none>".to_string(), f64::NEG_INFINITY),
        };
        crate::diag!(
            crate::observer::LogLevel::Info,
            "optimizer '{method}' on '{phase_name}' (control): best [{best_disp}] → \
             {objective}={best_value} after {evals} evals",
            method = block.method,
            objective = block.objective,
            evals = outcome.evals
        );
        Ok(())
    })
}

/// SRD-86 §4 hybrid actuation — a single optimize node mixing coordinate axes
/// (rerun) and control axes (servo). The coordinate axes form the OUTER rerun
/// grid; for each distinct coordinate cell the phase is re-run bound at that
/// cell, and the Control daemon servos the control subspace INTERIOR to it. The
/// node's `method`/`budget` drive the inner control search per cell; the
/// coordinate cells are enumerated (a continuous coordinate axis alongside a
/// control axis — the `IndexFn::Hybrid` shape — is a follow-up). The reported
/// best is `(coordinate-cell ; control-setting)` over all cells.
#[allow(clippy::too_many_arguments)] // mirrors `dispatch_optimization`'s shape
fn run_hybrid_search<'a>(
    ctx: &'a mut ExecCtx,
    space: crate::optimize::SearchSpace,
    coord_eval: CoordEval,
    control_axes: Vec<crate::optimize::servo::ControlAxis>,
    block: nmbrs_workload::model::OptimizeBlock,
    phase_name: &'a str,
    depth: usize,
    phase_meta: Option<(String, Vec<String>, Vec<crate::checkpoint::PathSegment>)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        require_windowed_objective(ctx, phase_name, &block.objective)?;

        // Partition axes: control indices (K, inner/servoed) vs the rest
        // (C, outer/rerun).
        let control_idx: std::collections::HashSet<usize> =
            control_axes.iter().map(|c| c.axis_idx).collect();
        let coord_indices: Vec<usize> = (0..space.axes.len())
            .filter(|i| !control_idx.contains(i))
            .collect();

        // The control subspace the daemon searches, with axis indices remapped
        // into K (the daemon proposes coordinates over K alone).
        let k_axes: Vec<crate::optimize::Axis> = control_axes
            .iter()
            .map(|ca| {
                let mut ax = space.axes[ca.axis_idx].clone();
                ax.changeover = crate::optimize::Changeover::Control;
                ax
            })
            .collect();
        let k_space = crate::optimize::SearchSpace::new(k_axes);
        let k_controls: Vec<crate::optimize::servo::ControlAxis> = control_axes
            .iter()
            .enumerate()
            .map(|(k_pos, ca)| crate::optimize::servo::ControlAxis {
                axis_idx: k_pos,
                control: ca.control.clone(),
            })
            .collect();

        // Enumerate the distinct coordinate cells (the outer rerun grid): project
        // each enumerated step onto the coordinate axes and dedup, keeping one
        // representative step per cell (its control values are a warmup the
        // daemon immediately overrides).
        let CoordEval::Enumerated { steps, .. } = &coord_eval else {
            return Err(format!(
                "phase '{phase_name}': hybrid coordinate+control optimize requires enumerated \
                 coordinate axes (a continuous coordinate axis alongside a control axis is not \
                 yet supported)"
            ));
        };
        let mut seen = std::collections::HashSet::new();
        let mut cells: Vec<IterationStep> = Vec::new();
        for step in steps {
            let key: String = coord_indices
                .iter()
                .map(|&i| step.bindings[i].1.to_display_string())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            if seen.insert(key) {
                cells.push(step.clone());
            }
        }

        // Per coordinate cell: re-run the phase bound at the cell and servo the
        // control subspace within it. Best is tracked across cells.
        let mut best_value = f64::NEG_INFINITY;
        let mut best_disp = "<none>".to_string();
        let mut total_evals = 0usize;
        for cell in &cells {
            let outcome = run_servo_cell(
                ctx,
                cell,
                k_space.clone(),
                k_controls.clone(),
                &block,
                phase_name,
                depth,
                &phase_meta,
            )
            .await?;
            total_evals += outcome.evals;
            if let Some(b) = &outcome.best
                && b.value > best_value
            {
                best_value = b.value;
                let c_disp = coord_indices
                    .iter()
                    .map(|&i| {
                        format!(
                            "{}={}",
                            cell.bindings[i].0,
                            cell.bindings[i].1.to_display_string()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let k_disp = b
                    .coord
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                best_disp = format!("{c_disp}; {k_disp}");
            }
        }
        crate::diag!(
            crate::observer::LogLevel::Info,
            "optimizer '{method}' on '{phase_name}' (hybrid {ncells} coordinate cells × control): \
             best [{best_disp}] → {objective}={best_value} after {total_evals} evals",
            method = block.method,
            objective = block.objective,
            ncells = cells.len()
        );
        Ok(())
    })
}

/// Push the per-evaluation scene node (mirrors `dispatch_comprehension`'s
/// per-iter Phase node) and run the iteration inline (sequential).
async fn run_one_eval(
    ctx: &mut ExecCtx,
    step: &IterationStep,
    phase_name: &str,
    depth: usize,
    phase_meta: &Option<(String, Vec<String>, Vec<crate::checkpoint::PathSegment>)>,
) -> Result<(), String> {
    let scene_id = match phase_meta {
        Some((pn, op_names, ypath)) => {
            let labels = canonical_phase_label(&step.coord_path);
            push_phase_scene_node(
                ctx.scene_tree_parent_id,
                ypath.clone(),
                pn.clone(),
                labels,
                op_names.clone(),
            )
        }
        None => ctx.scene_tree_parent_id,
    };
    let saved = ctx.scene_tree_parent_id;
    ctx.scene_tree_parent_id = scene_id;
    let terminal = TerminalAction::Phase(phase_name);
    let res = run_one_iteration(ctx, step, &terminal, depth, "optimize").await;
    ctx.scene_tree_parent_id = saved;
    // The optimize helpers stay `Result`-typed (niche subtree); map here.
    if res.is_failure() {
        Err(res.reason.clone().unwrap_or_default())
    } else {
        Ok(())
    }
}

/// Pull the next coordinate batch from a source via its most-capable
/// decorator (feedback favored over pull), mirroring the contract driver.
pub(crate) fn source_next(
    src: &mut Box<dyn crate::optimize::CoordinateSource>,
    evaluated: &[(crate::optimize::Coord, f64)],
) -> Option<Vec<crate::optimize::Coord>> {
    if let Some(f) = src.as_feedback() {
        f.step(evaluated)
    } else if let Some(p) = src.as_pull() {
        p.pull()
    } else {
        None
    }
}

/// A stable per-coordinate key (per-axis display value) for matching an
/// optimizer-produced coordinate back to its enumerated `IterationStep`.
fn coord_key(coord: &crate::optimize::Coord) -> String {
    coord
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

/// Index the enumerated steps by their coordinate key.
fn index_steps(steps: &[IterationStep]) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        let key = step
            .bindings
            .iter()
            .map(|(_, v)| polydat_value_to_axis(v).to_string())
            .collect::<Vec<_>>()
            .join("\u{1f}");
        m.entry(key).or_insert(i);
    }
    m
}

/// Convert a polydat `Value` to the optimizer's `AxisValue`.
fn polydat_value_to_axis(v: &polydat::ast::Value) -> crate::optimize::AxisValue {
    use polydat::ast::Value;
    match v {
        Value::F64(f) => crate::optimize::AxisValue::Num(*f),
        Value::U64(u) => crate::optimize::AxisValue::Num(*u as f64),
        Value::Bool(b) => crate::optimize::AxisValue::Bool(*b),
        Value::Str(s) => crate::optimize::AxisValue::Label(s.to_string()),
        other => crate::optimize::AxisValue::Label(other.to_display_string()),
    }
}

/// Build the `SearchSpace` from the enumerated candidate grid (SRD-86 A12 — the
/// grid informs the space, it is not the node's representation): per axis, the
/// distinct values (first-seen order) become detents — `Discrete` when all
/// numeric, `Categorical` otherwise. (Continuous-sampling spaces are a
/// follow-up; they need comprehension `Source` decoding.)
fn search_space_from_steps(steps: &[IterationStep]) -> crate::optimize::SearchSpace {
    use crate::optimize::{Axis, AxisKind, AxisValue, Changeover, SearchSpace};
    let n_axes = steps.first().map(|s| s.bindings.len()).unwrap_or(0);
    let mut axes = Vec::with_capacity(n_axes);
    for a in 0..n_axes {
        let name = steps[0].bindings[a].0.clone();
        let mut seen: Vec<String> = Vec::new();
        let mut values: Vec<AxisValue> = Vec::new();
        let mut all_numeric = true;
        for step in steps {
            let av = polydat_value_to_axis(&step.bindings[a].1);
            let key = av.to_string();
            if !seen.contains(&key) {
                seen.push(key);
                if !matches!(av, AxisValue::Num(_)) {
                    all_numeric = false;
                }
                values.push(av);
            }
        }
        let kind = if all_numeric {
            AxisKind::Discrete { detents: values }
        } else {
            AxisKind::Categorical { options: values }
        };
        axes.push(Axis {
            name,
            kind,
            changeover: Changeover::Coordinate,
        });
    }
    SearchSpace::new(axes)
}

/// Convert an optimizer [`AxisValue`](crate::optimize::AxisValue) to the
/// polydat [`Value`](polydat::ast::Value) bound into an iteration kernel.
fn axis_value_to_polydat(v: &crate::optimize::AxisValue) -> polydat::ast::Value {
    use crate::optimize::AxisValue;
    match v {
        AxisValue::Num(f) => polydat::ast::Value::F64(*f),
        AxisValue::Bool(b) => polydat::ast::Value::Bool(*b),
        AxisValue::Label(s) => polydat::ast::Value::Str(s.as_str().into()),
    }
}

/// If `comp` is a **pure-continuous** comprehension (a `lo .. hi` float range —
/// `Source::ContinuousInterval`), its axis intervals in clause order; `None` for
/// discrete or mixed (the first scope handles pure-continuous axes — a mixed
/// continuous/Ext space is a follow-up). Read from the comprehension's static
/// metadata, so no enumeration (a continuous source yields no tuples).
fn continuous_axis_intervals(
    comp: &polydat::iteration::comprehension::Comprehension,
) -> Option<Vec<polydat::iteration::comprehension::Interval>> {
    use polydat::iteration::comprehension::IndexFn;
    match comp.metadata().index_addressable {
        Some(IndexFn::Continuous { intervals, .. }) => Some(intervals),
        _ => None,
    }
}

/// Build a continuous [`SearchSpace`](crate::optimize::SearchSpace) from axis
/// names zipped with their `(lo, hi)` intervals — the optimizer samples each
/// interval; nothing is enumerated.
fn search_space_continuous(
    axis_names: &[String],
    intervals: &[polydat::iteration::comprehension::Interval],
) -> crate::optimize::SearchSpace {
    use crate::optimize::{Axis, AxisKind, Changeover, SearchSpace};
    SearchSpace::new(
        axis_names
            .iter()
            .zip(intervals)
            .map(|(name, iv)| Axis {
                name: name.clone(),
                kind: AxisKind::Continuous {
                    lo: iv.lo,
                    hi: iv.hi,
                    min_step: 0.0,
                },
                changeover: Changeover::Coordinate,
            })
            .collect(),
    )
}

/// How a proposed optimizer coordinate becomes the [`IterationStep`] to run.
///
/// - `Enumerated` (discrete): the feasible set IS the pre-enumerated grid, so a
///   proposed coordinate is matched to its step and an off-grid coordinate is
///   infeasible (SRD-86 §"holes" — the grid carries feasibility/holes).
/// - `Synthesized` (continuous): no enumeration — the realized coordinate is
///   bound into a fresh iteration kernel via [`PolydatKernel::for_iteration`],
///   exactly as `runtime_iterate` materializes a comprehension tuple.
enum CoordEval {
    Enumerated {
        steps: Vec<IterationStep>,
        index: std::collections::HashMap<String, usize>,
    },
    Synthesized {
        axis_names: Vec<String>,
        canonical: std::sync::Arc<polydat::kernel::PolydatKernel>,
        parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
        parent_coords: Vec<polydat::kernel::ScopeCoord>,
    },
}

impl CoordEval {
    /// The [`IterationStep`] to run for a proposed coordinate, or `None` when an
    /// enumerated coordinate falls off the feasible grid.
    fn materialize(&self, coord: &crate::optimize::Coord) -> Option<IterationStep> {
        match self {
            CoordEval::Enumerated { steps, index } => {
                index.get(&coord_key(coord)).map(|&i| steps[i].clone())
            }
            CoordEval::Synthesized {
                axis_names,
                canonical,
                parent,
                parent_coords,
            } => {
                let tuple: Vec<(String, polydat::ast::Value)> = axis_names
                    .iter()
                    .zip(coord)
                    .map(|(n, av)| (n.clone(), axis_value_to_polydat(av)))
                    .collect();
                let bound_kernel =
                    polydat::kernel::PolydatKernel::for_iteration(canonical, parent, &tuple);
                let mut coord_path = parent_coords.clone();
                coord_path.push(polydat::kernel::ScopeCoord::from(tuple.iter().cloned()));
                Some(IterationStep {
                    bindings: tuple,
                    bound_kernel,
                    coord_path,
                })
            }
        }
    }

    /// A representative [`IterationStep`] for the structural pre-map pass: the
    /// first enumerated step, or the continuous space's centre.
    fn representative(&self, space: &crate::optimize::SearchSpace) -> Option<IterationStep> {
        match self {
            CoordEval::Enumerated { steps, .. } => steps.first().cloned(),
            CoordEval::Synthesized { .. } => self.materialize(&space.center()),
        }
    }
}

/// SRD-86 A10 — read an optimize objective wire POST-EXECUTION via the canonical
/// completion-pull. The single phase kernel that ran was consumed by
/// `OpBuilder`, so (as `emit_phase_metrics` does) we rebuild a completion view —
/// here off the **live parent** kernel (carrying this iteration's coordinate)
/// with the phase node's program. Because it runs after the ops, `metric(...)`
/// reads the run's produced values. `catch_unwind` keeps a bad name a clean
/// error rather than a fiber-pool panic.
/// Read a *coordinate-function* objective wire after the phase has
/// executed.
///
/// SRD-86 A10. Node X's kernel at runtime *is* node X's program
/// evaluated as a subscope of its live parent scope: the parent
/// (`current_parent_kernel` = the iteration step's bound kernel)
/// carries the per-eval coordinate, and node X's program defines the
/// objective transform that reads it. So the completion read rebuilds
/// exactly that — node X's program (`phase_kernel.program()`) bound
/// to the live parent — and pulls the objective. This is not a
/// context-free clone: the coordinate resolves through the parent
/// scope (verified by `optimizer_null_sweep`, which regresses the
/// moment the build is reparented onto the cached phase template,
/// which holds no per-eval coordinate).
///
/// This path is correct for objectives that are a deterministic
/// function of the coordinate (and of session-stable reads). A
/// *volatile* objective over a windowed metric (`metric_window`)
/// settles to an empty trailing window here and must instead be read
/// from the cadence-fed settle daemon's register — see SRD-86
/// §"Settling via the cadence pulse". The caller selects the path by
/// objective shape.
fn read_objective_at_completion(
    parent: &std::sync::Arc<polydat::kernel::PolydatKernel>,
    phase_kernel: &std::sync::Arc<polydat::kernel::PolydatKernel>,
    objective: &str,
) -> Option<f64> {
    use polydat::ast::Value;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut k = parent
            .build_subscope(
                polydat::kernel::subcontext::PolydatMatter::builder()
                    .program(phase_kernel.program().clone())
                    .build()
                    .ok()?,
            )
            .ok()?;
        Some(k.pull(objective).clone())
    }));
    match result {
        Ok(Some(Value::F64(f))) => Some(f),
        Ok(Some(Value::U64(u))) => Some(u as f64),
        Ok(Some(Value::Bool(b))) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Route a `do_while` / `do_until` scenario node through the
/// streaming dispatcher (SRD-18b §"Deferred follow-on:
/// Streaming dispatch for state-driven comprehensions").
///
/// One persistent kernel for the whole loop's lifetime — forked
/// from the do-loop scope's canonical program at entry, bound to
/// the parent scope once. Each iteration:
///
/// 1. `set_input` the counter on the persistent kernel.
/// 2. Evaluate the condition against the persistent kernel via
///    the standard `interpolate_via_kernel` + `eval_const_expr`
///    path. Halt if the predicate flips.
/// 3. Wrap the persistent kernel in `Arc` and install it as
///    `ctx.current_parent_kernel`. Run children — they read the
///    loop kernel through the standard scope chain.
/// 4. After children return, reclaim the kernel via
///    `Arc::try_unwrap`. Iterations are sequential by design
///    (state-dependent), so the Arc is uniquely owned by the
///    time we get here. Concurrent siblings within an iteration
///    body each cloned the Arc but their tasks have all
///    awaited.
/// 5. Increment counter, repeat.
///
/// Shared writes from children's leaf kernels back to the
/// persistent loop kernel are not yet wired (Stage 2 of the
/// follow-on); the existing per-iteration fork model in
/// `run_one_iteration` doesn't surface a propagate hook to the
/// streaming dispatcher. Conditions that depend purely on the
/// counter (`do_while: counter < 100`) work today; conditions
/// that depend on `shared`-modifier state mutated by children
/// are still pending.
/// Fire a scope lifecycle event (ScopeStart / ScopeEnd)
/// for a non-iteration scope group (do_while / do_until).
/// Subject id is the scope's spec text so the snapshot
/// store distinguishes nested loops.
fn fire_scope_lifecycle(
    ctx: &ExecCtx,
    event: crate::lifecycle::EventType,
    spec: &str,
    depth: usize,
) {
    let depth_indent = " ".repeat(depth.saturating_sub(1));
    let display_labels: String = {
        let parent_coords: Vec<_> = ctx
            .current_parent_kernel
            .as_ref()
            .map(|k| k.scope_coordinates().iter().rev().cloned().collect())
            .unwrap_or_default();
        polydat::kernel::format_scope_coordinate_path(&parent_coords)
    };
    let scope_ctx = crate::readout_context::LifecycleContext {
        event,
        subject_name: spec.to_string(),
        subject_labels: display_labels,
        depth_indent,
        use_color: crate::observer::use_color(),
        stick_reattached: String::new(),
    };
    crate::readout_context::fire_lifecycle(
        event,
        &ctx.workload_readouts,
        None,
        &scope_ctx,
        Some(&ctx.sqlite_reporter),
    );
}

async fn run_do_loop(
    ctx: &mut ExecCtx,
    condition: &str,
    counter: Option<&str>,
    invert: bool,
    children: &[ScenarioNode],
    depth: usize,
) -> Result<(), String> {
    // Find the matching scope-tree node by structural content.
    // Both DoWhile and DoUntil get installed kernels per the
    // runner.rs install loop, but they're stored under their
    // own ScopeKind variants so the lookup needs to check both.
    let scope_idx = ctx
        .scope_tree
        .iter_dfs()
        .find_map(|(idx, node)| match &node.kind {
            crate::scope_tree::ScopeKind::DoWhile {
                condition: c,
                counter: ct,
            } => {
                if c == condition && ct.as_deref() == counter && !invert {
                    Some(idx)
                } else {
                    None
                }
            }
            crate::scope_tree::ScopeKind::DoUntil {
                condition: c,
                counter: ct,
            } => {
                if c == condition && ct.as_deref() == counter && invert {
                    Some(idx)
                } else {
                    None
                }
            }
            _ => None,
        })
        .ok_or_else(|| {
            format!(
                "do-loop '{condition}': no matching scope-tree entry — \
         scenario/scope-tree drift bug."
            )
        })?;
    let canonical = ctx.scope_tree.nodes[scope_idx]
        .cached_kernel
        .get()
        .cloned()
        .ok_or_else(|| {
            format!("do-loop '{condition}': scope at index {scope_idx} has no installed kernel.")
        })?;
    let parent = effective_parent_kernel(ctx, scope_idx)
        .ok_or_else(|| format!("do-loop '{condition}': no installed ancestor kernel."))?;

    // Persistent loop kernel: one fork from the do-loop scope's
    // canonical program, bound once from the parent. Lives for
    // the loop's whole duration. SRD-67 Phase 3 — route the
    // `from_program → materialize_wiring_from_outer` sequence through the
    // typed bridge so the rebind primitive sits behind a single
    // entry point.
    let mut loop_kernel = parent
        .build_subscope(
            polydat::kernel::subcontext::PolydatMatter::builder()
                .program(canonical.program().clone())
                .build()
                .unwrap(),
        )
        .expect("subscope from program is infallible");

    let mut counter_value: u64 = 0;
    loop {
        // Set the counter on the persistent kernel.
        if let Some(c) = counter
            && let Some(idx) = loop_kernel.program().find_input(c)
        {
            loop_kernel
                .state()
                .set_input(idx, polydat::ast::Value::U64(counter_value));
        }

        // Evaluate the condition against the persistent kernel.
        let interpolated = polydat::kernel::interp::interpolate_via_kernel(condition, &loop_kernel)
            .map_err(|e| format!("do-loop condition '{condition}': {e}"))?;
        let cond_value = polydat::dsl::compile::eval_const_expr(&interpolated)
            .map_err(|e| format!("do-loop condition '{condition}': {e}"))?;
        let cond_true = match cond_value {
            polydat::ast::Value::Bool(b) => b,
            polydat::ast::Value::U64(n) => n != 0,
            polydat::ast::Value::F64(n) => n != 0.0,
            other => {
                return Err(format!(
                    "do-loop condition '{condition}': expected bool/u64/f64, got {other:?}",
                ));
            }
        };
        let should_continue = if invert { !cond_true } else { cond_true };
        if !should_continue {
            break;
        }

        // Install the persistent kernel as ctx.current_parent_kernel
        // for children to read via the standard scope chain. We
        // move it into the Arc temporarily, then reclaim it after
        // children return.
        let prior_parent = ctx.current_parent_kernel.take();
        if let Some(c) = counter {
            ctx.push_label(c, &counter_value.to_string());
        }
        let arc_loop = std::sync::Arc::new(std::mem::replace(
            &mut loop_kernel,
            // Placeholder — overwritten on reclaim. Constructed
            // via the typed subscope path against the canonical;
            // shares cells but is otherwise throwaway.
            canonical
                .build_subscope(
                    polydat::kernel::subcontext::PolydatMatter::builder()
                        .program(canonical.program().clone())
                        .build()
                        .unwrap(),
                )
                .expect("program-form subscope is infallible"),
        ));
        ctx.current_parent_kernel = Some(arc_loop.clone());

        // SRD-44a Push 3 — emit `scope_enter` for THIS do-loop
        // iteration. Coords carry the counter binding (when one
        // is declared); path captures the enclosing scope chain
        // so resume can locate which loop, in which outer
        // iteration, was mid-flight on crash.
        let kind: &'static str = if invert { "do_until" } else { "do_while" };
        let mut iter_coords = std::collections::BTreeMap::new();
        if let Some(c) = counter {
            iter_coords.insert(c.to_string(), serde_json::Value::from(counter_value));
        }
        let path: Vec<std::collections::BTreeMap<String, serde_json::Value>> = arc_loop
            .scope_coordinates()
            .iter()
            .rev()
            .filter(|c| !c.is_empty())
            .map(coord_to_btree)
            .collect();
        if let Some(writer) = ctx.checkpoint_writer.as_ref() {
            writer.emit_scope_enter(kind, iter_coords.clone(), path.clone());
        }

        let res = execute_tree_at(ctx, children, depth).await;

        if let Some(writer) = ctx.checkpoint_writer.as_ref() {
            let outcome = if !res.is_failure() {
                "completed"
            } else {
                "interrupted"
            };
            writer.emit_scope_exit(kind, iter_coords, path, outcome);
        }

        ctx.current_parent_kernel = prior_parent;
        if counter.is_some() {
            ctx.pop_label();
        }

        // Reclaim the persistent kernel. Iterations are sequential
        // and concurrent children within the iteration body have
        // all awaited by this point, so the Arc is uniquely owned.
        loop_kernel = std::sync::Arc::try_unwrap(arc_loop).map_err(|_| {
            format!(
                "do-loop '{condition}' iteration {counter_value}: persistent kernel \
             still referenced after children completed — concurrency bug."
            )
        })?;

        if res.is_failure() {
            return Err(res.reason.clone().unwrap_or_default());
        }
        counter_value = counter_value.saturating_add(1);
    }

    Ok(())
}

/// Run one comprehension iteration: build branch kernel, set
/// inputs, push as `current_parent_kernel`, run the terminal
/// action (children tree-walk or single phase), restore.
/// Shared by serial and concurrent dispatch paths.
/// Drive one iteration of a comprehension dispatch: install the
/// pre-built bound kernel as the live parent, push iteration
/// labels for diagnostics, then descend through the terminal
/// action.
///
/// The bound kernel comes from the Polydat-side
/// [`IterationStep`] — same
/// kernel both pre-map and runtime see for the same iteration
/// position. No `from_program`/`materialize_wiring_from_outer`/`set_input`
/// dance here; that recipe is owned by `PolydatKernel::for_iteration`
/// and reached via `iterate_scope`.
async fn run_one_iteration(
    ctx: &mut ExecCtx,
    step: &IterationStep,
    terminal: &TerminalAction<'_>,
    depth: usize,
    kind: &'static str,
) -> crate::phase_outcome::Outcome {
    let prior_parent = ctx.current_parent_kernel.take();
    ctx.current_parent_kernel = Some(step.bound_kernel.clone());
    for (var, value) in &step.bindings {
        ctx.push_label(var, &value.to_display_string());
    }

    // SRD-44a Push 3 — emit `scope_enter` for THIS iteration of
    // the comprehension. The walker enters one scope per iteration
    // (each iteration's coordinates pin a unique sub-tree); the
    // matching `scope_exit` fires on the way out below regardless
    // of whether the body succeeded.
    let (enter_coords, enter_path) = scope_event_coords(&step.coord_path);
    if let Some(writer) = ctx.checkpoint_writer.as_ref() {
        writer.emit_scope_enter(kind, enter_coords.clone(), enter_path.clone());
    }

    // SRD-63 Push 9a: fire `EventType::EachStart` for this
    // iteration. Bindings carry the iteration tuple
    // (e.g. `(profile, alpha)`); the scope subject id is
    // the binding tuple as a sortable string. Subject
    // labels are the root-first display form a workload-
    // bound `scope_header` would render against.
    let iter_label = step
        .bindings
        .iter()
        .map(|(k, v)| format!("{k}={}", v.to_display_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let display_labels: String = {
        let parent_coords: Vec<_> = ctx
            .current_parent_kernel
            .as_ref()
            .map(|k| k.scope_coordinates().iter().rev().cloned().collect())
            .unwrap_or_default();
        polydat::kernel::format_scope_coordinate_path(&parent_coords)
    };
    let depth_indent = " ".repeat(depth.saturating_sub(1));
    let each_ctx = crate::readout_context::LifecycleContext {
        event: crate::lifecycle::EventType::EachStart,
        subject_name: iter_label.clone(),
        subject_labels: display_labels.clone(),
        depth_indent: depth_indent.clone(),
        use_color: crate::observer::use_color(),
        stick_reattached: String::new(),
    };
    crate::readout_context::fire_lifecycle(
        crate::lifecycle::EventType::EachStart,
        &ctx.workload_readouts,
        None,
        &each_ctx,
        Some(&ctx.sqlite_reporter),
    );

    // Children downstream consume iter-var values via
    // `ctx.current_parent_kernel` (set above), no separate
    // HashMap parameter.
    let res = match terminal {
        TerminalAction::Children(children) => execute_tree_at(ctx, children, depth).await,
        TerminalAction::Phase(name) => {
            // Depth gating: structural Phase scene node was
            // pushed by `dispatch_comprehension` before
            // spawning this iteration; the executional
            // `run_phase` only runs at depth >= Op. SRD 18b
            // §"Single Walker Contract" point 3.
            if ctx.diag.depth >= crate::runner::ExecDepth::Op {
                // SRD-100 P1c — for a Phase terminal, the dispatcher
                // (comprehension / optimize) set this ctx's
                // `scene_tree_parent_id` to the per-iter Phase node
                // itself before spawning the iteration, so it IS this
                // phase's dispatch-time row key.
                let phase_node_id = ctx.scene_tree_parent_id;
                run_phase_layered(ctx, name, phase_node_id).await
            } else {
                crate::phase_outcome::Outcome::skipped()
            }
        }
    };

    // SRD-63 Push 9a: fire `EventType::EachEnd` after the
    // iteration body returns (success or failure — the
    // scope did still complete its iteration step). The
    // ctx.subject_id() matches the start fire's so the
    // snapshot store collapses both into the latest
    // end-render.
    let each_end_ctx = crate::readout_context::LifecycleContext {
        event: crate::lifecycle::EventType::EachEnd,
        subject_name: iter_label,
        subject_labels: display_labels,
        depth_indent,
        use_color: crate::observer::use_color(),
        stick_reattached: String::new(),
    };
    crate::readout_context::fire_lifecycle(
        crate::lifecycle::EventType::EachEnd,
        &ctx.workload_readouts,
        None,
        &each_end_ctx,
        Some(&ctx.sqlite_reporter),
    );

    // SRD-44a Push 3 — mirror `scope_enter` with `scope_exit`,
    // tagged with the iteration's outcome. `completed` when the
    // terminal action returned `Ok`, `interrupted` when the body
    // errored or was unwound (a stop signal propagates as an
    // `Err` here).
    if let Some(writer) = ctx.checkpoint_writer.as_ref() {
        let outcome = if !res.is_failure() {
            "completed"
        } else {
            "interrupted"
        };
        writer.emit_scope_exit(kind, enter_coords, enter_path, outcome);
    }

    for _ in &step.bindings {
        ctx.pop_label();
    }
    ctx.current_parent_kernel = prior_parent;
    res
}

/// Translate an `IterationStep`'s root-first coord chain into
/// the SRD-44a `scope_enter` / `scope_exit` shape: the chain's
/// last entry becomes the event's `coords` (THIS iteration's
/// own bindings); everything before it, reversed, becomes the
/// leaf-first `path` of enclosing scopes' coords.
///
/// Empty `ScopeCoord` entries (scenario nodes that own no
/// comprehension vars) are filtered from `path` so a chain that
/// passes through a non-iterating scope doesn't render an empty
/// `{}` on disk — same convention
/// [`format_scope_coordinate_path`] uses for human display.
fn scope_event_coords(
    coord_path: &[polydat::kernel::ScopeCoord],
) -> (
    std::collections::BTreeMap<String, serde_json::Value>,
    Vec<std::collections::BTreeMap<String, serde_json::Value>>,
) {
    let coords = coord_path.last().map(coord_to_btree).unwrap_or_default();
    let path: Vec<_> = if coord_path.len() <= 1 {
        Vec::new()
    } else {
        coord_path[..coord_path.len() - 1]
            .iter()
            .rev()
            .filter(|c| !c.is_empty())
            .map(coord_to_btree)
            .collect()
    };
    (coords, path)
}

fn coord_to_btree(
    coord: &polydat::kernel::ScopeCoord,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    coord
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), v.to_json_value()))
        .collect()
}

/// Empty-bindings sentinel used by `run_phase` when the
/// kernel-routed M3.4 path is active. Iteration vars come via
/// the parent kernel's manifest in that case, so `build_scope`
/// gets an empty iteration_vars map and skips its
/// `add_iteration_var` injection.
static EMPTY_BINDINGS: std::sync::LazyLock<HashMap<String, String>> =
    std::sync::LazyLock::new(HashMap::new);

/// Execute one phase. Iteration-variable values come from the
/// `current_parent_kernel` manifest — every name visible at this
/// phase's enclosing scope is reachable via the standard Polydat API
/// on that one kernel. The legacy `bindings: HashMap` parameter
/// is gone (M3.4b).
/// Run a phase. Thin wrapper that scopes the ambient executing-phase
/// task-local ([`crate::execution_context::with_current_phase`]) for the whole
/// body, so every emit inside — the activity loop and the fibers it
/// `propagate`s — resolves `running_phase_indent` to THIS phase's depth under
/// concurrency (SRD-100 P1c). [`run_phase_inner`] carries the logic.
async fn run_phase(
    ctx: &mut ExecCtx,
    phase_name: &str,
    scene_node_id: crate::scene_tree::SceneNodeId,
) -> crate::phase_outcome::Outcome {
    // Phase-scoped clock origin, established HERE — the one place every phase
    // body passes through — so `phase_start_millis()` / `phase_elapsed_millis()`
    // resolve for the activity loop, its fibers, and the ops they run. Taken
    // before the body so it is the phase's start, not the first op's.
    let phase_start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let outcome = crate::execution_context::with_current_phase(
        scene_node_id,
        phase_start_ms,
        run_phase_inner(ctx, phase_name, scene_node_id),
    )
    .await;
    // Early config-resolution failures (`return Outcome::failed()`
    // before the failure epilogue — an unresolvable `concurrency:`,
    // a missing kernel, a bad cycles expression) used to leave the
    // walk blind: no observer event, no scene-tree outcome, no
    // workload-shell fold — so scenario stop-on-error never tripped,
    // sibling tiers raced on, and the reason surfaced exactly once
    // at process exit. This chokepoint (ONE place, covering every
    // early-return site) detects a failed Outcome that never got
    // recorded and routes it through the same visible surfaces as a
    // runtime failure.
    if outcome.is_failure() {
        let recorded =
            crate::scene_tree::with_global(|t| t.phase_outcome_present_at(scene_node_id))
                .unwrap_or(false);
        if !recorded {
            record_early_phase_failure(ctx, scene_node_id, phase_name, &outcome);
        }
    }
    outcome
}

/// Route an early (pre-epilogue) phase failure through the visible
/// surfaces: loud ERR line, observer, scene tree, checkpoint,
/// sqlite, and the workload-shell child fold so stop-on-error halts
/// the walk. Labels/duration are unavailable this early — the
/// identity is the phase name and the duration is zero.
fn record_early_phase_failure(
    ctx: &mut ExecCtx,
    scene_node_id: crate::scene_tree::SceneNodeId,
    phase_name: &str,
    outcome: &crate::phase_outcome::Outcome,
) {
    let reason = outcome
        .reason
        .clone()
        .unwrap_or_else(|| "phase configuration failed".to_string());
    crate::diag!(
        crate::observer::LogLevel::Error,
        "phase '{phase_name}': {reason} — failing phase (config)"
    );
    ctx.observer
        .phase_failed(scene_node_id, phase_name, "", &reason);
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let phase_outcome = crate::phase_outcome::PhaseOutcome::failed(
        crate::phase_outcome::PhaseIdentity::new(phase_name, ""),
        0.0,
        vec![crate::phase_outcome::PhaseErrorDetail {
            class: "config".into(),
            message: reason.clone(),
            op_name: None,
            cycle: None,
            op_template: None,
            op_resolved: None,
            at_nanos: now_nanos,
            retryable: false,
        }],
    );
    if let Ok(mut guard) = ctx.sqlite_reporter.lock()
        && let Some(reporter) = guard.as_mut()
    {
        let row = phase_outcome.to_sqlite_row(&ctx.session_id, ctx.exec_id, now_nanos as i64);
        reporter.write_phase_outcome(&row);
    }
    crate::scene_tree::with_global_mut(|t| {
        t.set_phase_failed_at(scene_node_id, &reason);
        t.set_phase_outcome_at(scene_node_id, phase_outcome);
    });
    if let Some(writer) = ctx.checkpoint_writer.as_ref() {
        let identity = phase_identity_for(phase_name, "");
        writer.phase_failed(&identity, &reason);
        let _ = writer.flush();
    }
    if let Some((trip_outcome, trip_reason)) = ctx.workload_shell.record_phase(true, 0, 0) {
        let cause = if trip_outcome.is_failure() {
            crate::session_signals::StopCause::Fault
        } else {
            crate::session_signals::StopCause::Interrupt
        };
        crate::session_signals::request_shell_stop(cause);
        crate::diag!(
            crate::observer::LogLevel::Warn,
            "scenario stop-on-error ({trip_reason}) after phase              '{phase_name}' — halting remaining walk"
        );
    }
}

async fn run_phase_inner(
    ctx: &mut ExecCtx,
    phase_name: &str,
    // SRD-100 P1c — this phase's dispatch-time scene-tree node id.
    // Allocated by the walker (`push_phase_scene_node`) before
    // `run_phase` is called and threaded through every lifecycle
    // flip + observer callback so status/duration/outcome land on
    // THIS phase's node, never a same-named sibling's (the
    // `find_phase`-by-DFS-order race).
    scene_node_id: crate::scene_tree::SceneNodeId,
) -> crate::phase_outcome::Outcome {
    // SRD-92 — run_phase produces its two-axis Outcome natively (per-exit).
    let phase_start = std::time::Instant::now();
    // SRD-76 — wall-clock start nanos for the `phase_outcomes`
    // row. Instant gives monotonic duration; SystemTime gives
    // the chronological anchor downstream consumers (replay,
    // metricsql correlation) need.
    let phase_start_nanos: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let phase = match ctx.phases.get(phase_name) {
        Some(p) => p.clone(),
        None => {
            return crate::phase_outcome::Outcome::failed()
                .with_reason(format!("phase '{phase_name}' not found"));
        }
    };
    let has_bindings = phase.ops.iter().any(|op| !op.bindings.is_empty());

    // Derive iteration-variable values from the current parent
    // kernel's namespace. Names visible at the parent scope —
    // own outputs (folded constants from `const` bindings) plus
    // inherited extern inputs (populated by `materialize_wiring_from_outer`
    // chain or per-iteration `set_input` from the dispatcher)
    // — flow through a single `PolydatKernel::lookup` call per name,
    // which is the canonical scope-aware reader (SRD-16
    // §"Visibility Rules: Shadowing"): folded outputs first,
    // then cell-aware input read, then `None`. Switching off
    // the older `get_constant` + `get_input` two-pass shape
    // closes the last call site that conflated the two read
    // tiers — Stage 2 of the params-kernel rework.
    //
    // iter_var_values must hold the FULL set of names visible
    // at the parent scope (own outputs + cascade-inherited
    // workload params + extern input slots) because it drives
    // both op-template `{name}` text substitution AND config
    // expression resolution downstream — dropping inherited
    // names here would leave `{keyspace}` etc. unresolved in
    // op templates. Display layers (phase_labels, scene tree)
    // filter to own names separately via
    // `program.own_output_names()` / `is_inherited`.
    //
    // IndexMap (insertion-ordered) so `phase_labels`,
    // `polydat_context`, and `activity_name` reflect scenario-tree
    // declaration order — `output_names()` / `input_names()`
    // already return Vecs in that order. A plain HashMap here
    // randomises iteration per process and produced visibly
    // jumbled multi-clause for_each labels.
    let iter_var_values: IndexMap<String, String> = {
        let mut out = IndexMap::new();
        if let Some(parent) = ctx.current_parent_kernel.as_ref() {
            for name in parent.program().output_names() {
                if let Some(v) = parent.lookup(name) {
                    out.insert(name.to_string(), v.to_display_string());
                }
            }
            for name in parent.program().input_names() {
                if out.contains_key(&name) {
                    continue;
                }
                if let Some(v) = parent.lookup(&name) {
                    out.insert(name.clone(), v.to_display_string());
                }
            }
        }
        out
    };
    let is_iter = !iter_var_values.is_empty();

    // The `=== phase: NAME ===` decoration is informational
    // chrome — the per-phase startup line below already names
    // the phase in a structured way that the inline status
    // thread, the post-run summary, and the TUI all consume.
    // Demoted to Debug so default Info-level stderr stays
    // hierarchically-clean; `loglevel=debug` brings it back
    // for callers that want the visual section break.
    crate::diag!(
        crate::observer::LogLevel::Debug,
        "=== phase: {phase_name} ==="
    );
    if is_iter && let Some(parent) = ctx.current_parent_kernel.as_ref() {
        let prog = parent.program();
        for (var, val) in &iter_var_values {
            if !val.is_empty() && !prog.is_inherited(var) {
                crate::diag!(crate::observer::LogLevel::Debug, "  {var}={val}");
            }
        }
    }

    // --- Resume short-circuit (SRD-44 §"Skip on resume") ---
    //
    // Consult the resume plan before the expensive compile +
    // dispatch work. A `Skip` action means a prior invocation
    // already completed this phase, identity matches, and the
    // operator declared it idempotent — so the cheapest correct
    // thing to do is mark it Completed in the scene tree and
    // return. Mismatches and ReRun fall through to the normal
    // path; CursorResume is wired in Tier 2.
    let early_phase_labels = match ctx.current_parent_kernel.as_ref() {
        Some(parent) => format_scope_coordinate_path(parent.scope_coordinates()),
        None => String::new(),
    };
    let early_identity = phase_identity_for(phase_name, &early_phase_labels);
    match ctx.resume_plan.action_for(&early_identity) {
        crate::checkpoint::ResumeAction::Skip if ctx.refine_plan.is_some() => {
            // SRD-106 D2 — under refine, the checkpoint plan's
            // Skip is anchored to the pre-map ancestor-chain
            // hash, which cannot see own-program edits (a param
            // interpolated into this phase's bindings, an op
            // template change). The deferred refine gate below
            // compares the FULL instance hash against the prior
            // outcome row, so it owns the skip decision here;
            // fall through to compile.
            crate::diag!(
                crate::observer::LogLevel::Info,
                "phase '{phase_name}' [checkpoint-resume skip deferred \
                 to the refine hash gate]"
            );
        }
        crate::checkpoint::ResumeAction::Skip => {
            // Surface the skip on the same observer + scene-tree
            // surfaces a normal phase uses, so the TUI tree, the
            // post-run summary, and session.log all show the
            // phase as Completed (with a sentinel zero duration
            // — there's no real wall-clock to report). Identity
            // is left None on the writer's existing entry; the
            // saved hash and duration from the prior run are
            // preserved verbatim.
            crate::diag!(
                crate::observer::LogLevel::Info,
                "phase '{phase_name}' [skipped — checkpoint resume]"
            );
            crate::scene_tree::with_global_mut(|t| {
                t.set_phase_running_at(scene_node_id, 0);
                t.set_phase_completed_at(scene_node_id, 0.0);
            });
            ctx.observer
                .phase_starting(scene_node_id, phase_name, &early_phase_labels, 0, 0, 0);
            ctx.observer
                .phase_completed(scene_node_id, phase_name, &early_phase_labels, 0.0);
            crate::phase_end_triggers::fire_phase_completed(phase_name, &early_phase_labels, 0.0);
            return crate::phase_outcome::Outcome::skipped();
        }
        crate::checkpoint::ResumeAction::IdentityMismatch { reason } => {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "phase '{phase_name}' [resume: {reason} — re-running]"
            );
        }
        crate::checkpoint::ResumeAction::CursorResume { .. } => {
            // Tier 2: source-factory restore_cursor wiring lands
            // alongside the cursor-state collector. For now log
            // the action and fall through to a clean ReRun so
            // the run continues correctly even though it pays
            // the duplicate-work cost.
            crate::diag!(
                crate::observer::LogLevel::Info,
                "phase '{phase_name}' [resume: cursor-state available — \
                 Tier 2 restore not yet wired, re-running from scratch]"
            );
        }
        crate::checkpoint::ResumeAction::ReRun => {}
    }

    // --- `skipped_phases=prune`: pre-entry gate short-circuit ---
    //
    // When EVERY op of the phase declares `if:` and every gate
    // evaluates false against the CURRENT parent chain (per-iteration
    // values included), the phase would run only to skip each cycle.
    // Prune mode skips the traversal itself: no activity, no fibers,
    // no observer events, no completion line — the scene tree records
    // the Skipped outcome for replay, and the walk moves on. Gates
    // are evaluated on per-iteration hydrations of the ops' installed
    // template kernels — the same programs the fibers would use.
    if crate::observer::skipped_phase_display() == crate::observer::SkippedPhaseDisplay::Prune
        && let Some(phase_def) = ctx.phases.get(phase_name)
        && !phase_def.ops.is_empty()
        && phase_def.ops.iter().all(|op| op.condition.is_some())
        && let Some(parent) = ctx.current_parent_kernel.as_ref()
    {
        // `ctx.current_scope_idx` is the ENCLOSING scenario scope (the
        // phase arm doesn't descend the cursor); resolve this phase's own
        // scope node among its children by name. Ambiguity (same-named
        // sibling phases) falls through to a normal run.
        let phase_nodes: Vec<crate::scope_tree::ScopeNodeIdx> = ctx.scope_tree.nodes
            [ctx.current_scope_idx]
            .children
            .iter()
            .copied()
            .filter(|c| {
                matches!(&ctx.scope_tree.nodes[*c].kind,
                    crate::scope_tree::ScopeKind::Phase { name } if name == phase_name)
            })
            .collect();
        let mut all_false = phase_nodes.len() == 1;
        let mut gates_checked = 0usize;
        let op_children: Vec<crate::scope_tree::ScopeNodeIdx> = phase_nodes
            .first()
            .map(|p| ctx.scope_tree.nodes[*p].children.clone())
            .unwrap_or_default();
        for child in &op_children {
            let node = &ctx.scope_tree.nodes[*child];
            let crate::scope_tree::ScopeKind::OpTemplate { name } = &node.kind else {
                continue;
            };
            let Some(op) = phase_def.ops.iter().find(|o| o.name == *name) else {
                continue;
            };
            let Some(cond) = op.condition.as_ref() else {
                continue;
            };
            let cond_name = cond.trim().trim_start_matches('{').trim_end_matches('}');
            let Some(installed) = node.cached_kernel.get() else {
                all_false = false; // no kernel to consult — don't guess
                break;
            };
            let gate_kernel = polydat::kernel::PolydatKernel::for_iteration(installed, parent, &[]);
            // `for_iteration` returns a fresh kernel; this Arc holds the
            // only reference, so the unwrap is structural. Fall through
            // to a normal run rather than guess if that ever changes.
            let Ok(mut gate_kernel) = std::sync::Arc::try_unwrap(gate_kernel) else {
                all_false = false;
                break;
            };
            let truthy = match gate_kernel.pull(cond_name) {
                polydat::ast::Value::None => false,
                polydat::ast::Value::U64(v) => *v != 0,
                polydat::ast::Value::F64(v) => *v != 0.0,
                polydat::ast::Value::Bool(v) => *v,
                polydat::ast::Value::Str(s) => !s.is_empty(),
                _ => true,
            };
            gates_checked += 1;
            if truthy {
                all_false = false;
                break;
            }
        }
        // Every op's gate must have been FOUND and evaluated false —
        // a scope-tree shape mismatch (wrong current_scope_idx, elided
        // op nodes) must fall through to a normal run, never prune.
        if all_false && gates_checked != phase_def.ops.len() {
            all_false = false;
        }
        if all_false {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "phase '{phase_name}' [pruned — every op gate false at entry \
                 (skipped_phases=prune)]"
            );
            crate::scene_tree::with_global_mut(|t| {
                t.set_phase_completed_at(scene_node_id, 0.0);
            });
            return crate::phase_outcome::Outcome::skipped();
        }
    }

    // SRD-108 backstop: a phase must reach dispatch with at least
    // one op. Load-time validation (parse Stage 6.5 selector
    // resolution, the binder's slot-coverage checks) makes this
    // unreachable for well-formed workloads; if some future load
    // path regresses, fail the PHASE with a structured outcome
    // instead of panicking a worker task inside `OpSequence`.
    if phase.ops.is_empty() {
        return crate::phase_outcome::Outcome::failed().with_reason(format!(
            "phase '{phase_name}' reached dispatch with no ops — \
             a load-time validation gap; every phase needs inline \
             ops, a matching `tags:` selector, or a bound \
             implementation",
        ));
    }

    // --- Compile inner kernel via BindingScope ---
    let (
        iter_op_builder,
        iter_ops,
        runtime_cursor_extents,
        runtime_cursor_min_ms,
        runtime_cursor_min_passes,
        runtime_cursor_min_count,
        runtime_cursor_delta,
        runtime_cursor_partition,
        activation_scope,
    ) = if is_iter || has_bindings {
        let mut ops = phase.ops.clone();

        // SRD-16 single read path: every `{name}` placeholder
        // in op fields and op-level params resolves through
        // the populated parent kernel's `lookup` interface —
        // the same surface that answers iter vars, cascaded
        // workload params, and any other in-scope name. There
        // is no parallel HashMap or fresh-state pull. Per-cycle
        // bindings declared in this phase's `bindings:` block
        // pass through (the dispenser resolves them at execute
        // time); anything else that doesn't resolve is a hard
        // error with the field path and the in-scope name list
        // surfaced to the operator.
        let parent_kernel = match ctx.current_parent_kernel.as_ref() {
            Some(k) => k,
            None => {
                return crate::phase_outcome::Outcome::failed().with_reason(format!(
                    "phase '{phase_name}': no current_parent_kernel — \
                 single-resolution-path requires the populated parent kernel",
                ));
            }
        };
        // SRD-68 Push 5c-cleanup: NO mutation of the workload
        // model. Op fields stay pristine (the dispenser handles
        // resolution at construction or cycle time per its
        // adapter shape); op params are pre-resolved at
        // wrapper construction time using the dispenser's own
        // canonical kernel (see validation.rs::wrap and the
        // sibling helpers in `crate::scope::resolve_placeholders_in_op_params`).
        // The workload-load step here just validates that every
        // referenced name resolves at the phase scope kernel —
        // unresolved references surface with a workload-load
        // diagnostic naming the field path and the in-scope
        // names. No text is rewritten.
        // SRD-13f Push D: validate against the phase scope-tree
        // kernel when one is installed — that kernel owns phase-
        // level bindings (`c := (cycle)`) so a placeholder like
        // `{c}` resolves there. Pre-Push-D, phase bindings were
        // merged into `op.bindings` at parse time, so
        // `collect_phase_binding_lhs_names(&ops)` (inside the
        // validator) recognised them as per-cycle declarations
        // and let the workload-root kernel pass validation. After
        // Push D phase bindings live only on their own scope's
        // kernel; validating against the workload root would
        // wrongly reject any op-field reference to a phase
        // binding.
        let validation_kernel = ctx
            .scope_tree
            .phase_node_by_name(phase_name)
            .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get())
            .map(|k| k.as_ref())
            .unwrap_or(parent_kernel);
        if let Err(e) = crate::scope::validate_placeholders_via_kernel(&ops, validation_kernel)
            .map_err(|e| format!("phase '{phase_name}': {e}"))
        {
            return crate::phase_outcome::Outcome::failed().with_reason(e);
        }

        // Rewrite inline expressions ({{expr}} → {__expr_N}) in op templates.
        // This modifies op template strings and returns the expr→name map
        // so the scope can register the corresponding bindings.
        crate::scope::rewrite_inline_exprs(&mut ops);

        // Note: per-op `adapter:` / `driver:` overrides are read
        // by the activity-layer dispenser-construction loop
        // (see `Activity::run_with_adapters` at the
        // `template.params.get("adapter")` site) and by the
        // executor's adapter-name collection (the
        // `for t in op_sequence.templates()` loop below). They
        // MUST survive into `op.params` for both paths to see
        // them — the previous strip here pre-dated per-op
        // adapter selection and silently dropped the field.

        // M3.4a: when the dependent-tuple dispatcher (or any
        // future kernel-routed enclosing scope) has installed a
        // per-branch parent kernel via
        // `ctx.current_parent_kernel`, use *that kernel's*
        // manifest as the auto-extern source. Iteration vars
        // are scope outputs of the parent (per M3.2's extern
        // auto-passthrough synthesis) and inherited workload
        // values flow through the same chain — one source of
        // resolvable values per SRD-16. The empty
        // `iteration_vars` map below means `build_scope`
        // doesn't separately call `add_iteration_var`; the
        // names auto-extern from the parent manifest with
        // their already-detected native types. When no parent
        // kernel is set (legacy non-dispatcher path), fall
        // M3.4b: every leaf phase compiles against its
        // immediate parent scope's manifest (via
        // `current_parent_kernel`). Iter vars are scope outputs
        // there; the empty `iteration_vars` map below skips
        // `add_iteration_var`'s typed-extern injection — the
        // names auto-extern from the parent's manifest with
        // their already-detected native types. Workload root is
        // always installed (M3.4b), so this branch always
        // fires; the legacy `outer_manifest` fallback is gone.
        let parent_kernel = match ctx.current_parent_kernel.as_ref() {
            Some(k) => k,
            None => {
                return crate::phase_outcome::Outcome::failed().with_reason(format!(
                    "phase '{phase_name}': no current_parent_kernel — \
                 workload root install missed at session start (internal bug)."
                ));
            }
        };
        // SRD-13f §"Wire-reference classification" — for case-3
        // local-inclusion the synthesizer needs the phase scope
        // kernel (which carries `phase.bindings` as AST) when one
        // was installed; otherwise the immediate runtime parent
        // (current_parent_kernel) is the right resolver. Same
        // lookup pattern as the placeholder validator above.
        let classifier_kernel: &polydat::kernel::PolydatKernel = ctx
            .scope_tree
            .phase_node_by_name(phase_name)
            .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get())
            .map(|k| k.as_ref())
            .unwrap_or(parent_kernel);
        let effective_manifest = crate::runner::extract_manifest(classifier_kernel.program());

        // Build typed scope from structured inputs. M3.6:
        // phase-level scope passes empty workload_params —
        // those are now scope outputs of the workload kernel
        // (declared as `const` bindings at compile) and reach
        // this phase via the parent-kernel manifest's
        // auto-extern. Local injection here would just create
        // duplicate locals.
        let scope = match crate::scope::build_scope(
            &ops,
            &EMPTY_BINDINGS,
            &effective_manifest,
            &EMPTY_BINDINGS,
            &ctx.phases,
            phase.cycles.as_deref(),
            &[], // exclude
            Some(classifier_kernel),
        ) {
            Ok(v) => v,
            Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
        };

        // Validate scope rules (shadow detection, final checks)
        let polydat_context = if iter_var_values.is_empty() {
            format!("phase '{phase_name}'")
        } else {
            let vars: Vec<String> = iter_var_values
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!("phase '{phase_name}' ({})", vars.join(", "))
        };
        if let Err(e) = scope
            .validate()
            .map_err(|e| format!("{polydat_context}: {e}"))
        {
            return crate::phase_outcome::Outcome::failed().with_reason(e);
        }

        // Compile-and-cache or rebind path (SRD 18b §"Cache-and-
        // rebind contract").
        //
        // The phase scope's `Arc<PolydatProgram>` lives in a
        // `OnceLock` on its scope-tree node. First call compiles
        // (using the chain-walked pragmas), inserts; subsequent
        // calls retrieve and build a fresh `PolydatKernel` from the
        // cached program with a freshly-created `GkState`. Each
        // call ends up with the same shape — a populated
        // `PolydatKernel` ready for outer-scope and iteration-variable
        // extern injection — but only the first call pays the
        // compile cost.
        let cursor_limit: Option<u64> = ctx.merged_params.get("limit").and_then(|s| s.parse().ok());
        let phase_idx = ctx.scope_tree.phase_node_by_name(phase_name);
        let phase_pragmas = phase_idx
            .map(|idx| ctx.scope_tree.nodes[idx].pragmas.clone())
            .unwrap_or_default();

        // Resolve the phase scope's program (compile-and-cache
        // on first hit) and rebind it under the parent kernel.
        // SRD-67 Phase 3 — the `from_program → materialize_wiring_from_outer`
        // pair routes through `bind_program_under_parent` so the
        // cache-and-rebind primitive sits behind a single typed
        // entry point.
        let phase_program = if let Some(idx) = phase_idx {
            let node = &ctx.scope_tree.nodes[idx];
            if let Some(canonical) = node.cached_kernel.get() {
                canonical.program().clone()
            } else {
                // First call for this phase — compile, install
                // the just-compiled kernel as this scope's
                // canonical instance (so `lookup_name` can read
                // its folded constants). Subsequent iterations
                // all hit the OnceLock cache hit branch above.
                // The program is iter-invariant (iter vars flow
                // as wires; dataset specs interpolate at eval),
                // so the same program serves every iteration.
                let compiled = match crate::bindings::compile_from_scope(
                    &scope,
                    ctx.workload_dir.as_deref(),
                    ctx.polydat_lib_paths.clone(),
                    ctx.strict,
                    &polydat_context,
                    cursor_limit,
                    &phase_pragmas,
                )
                .map_err(|e| format!("{polydat_context}: {e}"))
                {
                    Ok(v) => v,
                    Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
                };
                let prog = compiled.program().clone();
                let _ = node.cached_kernel.set(std::sync::Arc::new(compiled));
                prog
            }
        } else {
            // Phase not in the scope tree (shouldn't happen for
            // any executor-driven invocation; defensive). Fall
            // back to the un-cached compile path.
            match crate::bindings::compile_from_scope(
                &scope,
                ctx.workload_dir.as_deref(),
                ctx.polydat_lib_paths.clone(),
                ctx.strict,
                &polydat_context,
                cursor_limit,
                &phase_pragmas,
            )
            .map_err(|e| format!("{polydat_context}: {e}"))
            {
                Ok(v) => v,
                Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
            }
            .program()
            .clone()
        };

        // Wire inherited values + iter-var values from the
        // parent scope's per-branch kernel via standard GK
        // chain composition. Single call, single source of
        // values — SRD-16 §"Visibility Rules".
        let mut kernel = parent_kernel
            .build_subscope(
                polydat::kernel::subcontext::PolydatMatter::builder()
                    .program(phase_program)
                    .build()
                    .unwrap(),
            )
            .expect("program-form subscope is infallible");

        // ─── Plan B: Init-Binding Contract (scope-activation) ─────
        //
        // SRD 11 §"Init Binding Contract" Plan B: every binding
        // declared `init` must materialize as a single concrete
        // value once iteration externs are populated. We pull each
        // one on the activation kernel's state — that runs the
        // eval exactly once per scope activation — and verify the
        // result is non-None. The pulled values are captured into
        // OpBuilder.init_overrides below so per-fiber states inherit
        // them without re-evaluating.
        //
        // Plan A (in fold_init_constants_impl) caught structural
        // violations at compile time; Plan B catches runtime
        // failures (eval panic via catch_unwind, fatal Value::None
        // returns, missing output_map entry).
        let const_outputs: Vec<String> = kernel
            .program()
            .const_outputs()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for init_name in &const_outputs {
            // catch_unwind so a panicking eval becomes a clean error,
            // not a fiber-pool poisoning panic. Nodes that do blocking
            // I/O are responsible for parking the worker themselves
            // (see `polydat`'s `run_blocking_io`); the activation
            // boundary stays a plain eval.
            let pull_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                kernel.pull(init_name).clone()
            }));
            match pull_result {
                Ok(v) if !matches!(v, polydat::ast::Value::None) => {}
                Ok(_) => {
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "{polydat_context}: init binding '{init_name}' violates the init contract: \
                         scope-init eval returned Value::None (per SRD 11 §\"Init Binding Contract\" \
                         Plan B). The eval function signaled failure or returned no value."
                    ));
                }
                Err(payload) => {
                    let msg = panic_message(&payload);
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "{polydat_context}: init binding '{init_name}' violates the init contract: \
                         scope-init eval panicked: {msg} (per SRD 11 §\"Init Binding Contract\" \
                         Plan B)."
                    ));
                }
            }
        }

        // ─── Scope-activation pull for unfolded `final` outputs ───
        //
        // `const NAME := <expr>` declares "this value is fixed for
        // the scope's lifetime." When the RHS is a literal or
        // depends only on other compile-time constants, the GK
        // compiler const-folds the value into the program's
        // buffer and `get_constant` returns it immediately.
        //
        // But `const` bindings with RHS depending on iter-vars
        // or other extern slots (`const ann_opts := select_str(
        // ..., "WITH ann_options = {limit}", ...)` where `limit`
        // is an iter-var) can't fold at compile — the extern
        // value isn't known until materialize-wiring binds it
        // per scope activation. The unfolded result is a runtime
        // node whose buffer stays `Value::None` until something
        // pulls it.
        //
        // Construction-time consumers like the CQL adapter's
        // structural-resolve pass call `kernel.lookup(name)` and
        // expect to see the final's value. Without this pre-pull,
        // they get `None` → fall back to `?` bind-point — which
        // produces broken CQL when the placeholder isn't a value
        // position (e.g. the trailing `WITH ann_options = …`
        // clause after `LIMIT N`).
        //
        // Pull every `final` output whose buffer is currently
        // None; this realises the "fixed for the scope" contract
        // at scope-init time, after iter-vars and externs are
        // populated. Same panic/None handling as init bindings —
        // a `final` whose runtime eval fails is just as much a
        // contract violation as an `init` that does.
        let const_outputs: Vec<String> = kernel
            .program()
            .const_outputs()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for final_name in &const_outputs {
            if kernel.get_constant(final_name).is_some() {
                continue;
            }
            let pull_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                kernel.pull(final_name).clone()
            }));
            match pull_result {
                Ok(v) if !matches!(v, polydat::ast::Value::None) => {}
                Ok(_) => {
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "{polydat_context}: final binding '{final_name}' could not be \
                         materialised at scope activation: eval returned Value::None. \
                         If the RHS depends on a wire that's only available per cycle, \
                         use a non-modifier cycle binding (`{final_name} := …`) instead \
                         of `final`."
                    ));
                }
                Err(payload) => {
                    let msg = panic_message(&payload);
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "{polydat_context}: final binding '{final_name}' eval panicked at \
                         scope activation: {msg}"
                    ));
                }
            }
        }
        // ───────────────────────────────────────────────────────────

        // SRD-71 P3 — apply phase-scoped CLI parameter overrides
        // before anything reads params on this kernel (the cursor
        // `over <param>` pull below is the primary consumer).
        // Resolution per param: exact phase name beats glob;
        // distinct globs both matching is fatal. The write lands
        // on this phase's local slot, so the standard scope chain
        // serves the overridden value to everything below. A
        // literal-pattern override naming a param this phase has
        // no slot for gets a warning (the operator named THIS
        // phase); glob matches skip silently — a glob
        // legitimately spans phases that don't all consume the
        // param.
        if !ctx.phase_param_overrides.is_empty() {
            let chosen = match crate::phase_params::resolve_for_phase(
                &ctx.phase_param_overrides,
                phase_name,
            )
            .map_err(|e| format!("{polydat_context}: {e}"))
            {
                Ok(v) => v,
                Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
            };
            for (ov, _dialect) in chosen {
                use polydat::kernel::{Dataflow, WriteError};
                match kernel.set_wire(&ov.param, polydat::ast::Value::Str(ov.value.clone().into()))
                {
                    Ok(()) => {
                        crate::diag!(
                            crate::observer::LogLevel::Info,
                            "phase '{phase_name}': param `{}` overridden to `{}` \
                             (CLI `{}.{}=`)",
                            ov.param,
                            ov.value,
                            ov.pattern.source(),
                            ov.param
                        );
                    }
                    Err(WriteError::UnknownWire { .. }) => {
                        if ov.pattern.is_exact_literal() {
                            crate::diag!(
                                crate::observer::LogLevel::Warn,
                                "phase '{phase_name}': override `{}.{}=` names a \
                                 param this phase does not consume — no wire \
                                 named `{}` in its scope",
                                ov.pattern.source(),
                                ov.param,
                                ov.param
                            );
                        }
                    }
                    Err(e) => {
                        return crate::phase_outcome::Outcome::failed().with_reason(format!(
                            "{polydat_context}: phase-scoped override `{}.{}=`: {e}",
                            ov.pattern.source(),
                            ov.param,
                        ));
                    }
                }
            }
        }

        // Resolve cursor extents whose `range(...)` bounds depend on
        // wire-bound externs (e.g., `vector_count("{dataset}:{profile}")`
        // where `dataset` and `profile` are iter-var externs). The
        // compiler couldn't const-fold these, so it stashed the aux
        // output names on the schema's `extent_outputs`. Now that the
        // externs are populated on `kernel.state`, pull each pair and
        // record the resolved extent — keyed by cursor name — for the
        // source-factory construction below.
        let mut runtime_extents: HashMap<String, u64> = HashMap::new();
        // Resolved policy-arg buckets, keyed by cursor name.
        // The source-factory site reads from these to build the
        // appropriate ExtensionPolicy per CursorKind. Pulled
        // here while `kernel` is in scope.
        let mut runtime_min_ms: HashMap<String, u64> = HashMap::new();
        let mut runtime_min_passes: HashMap<String, u64> = HashMap::new();
        let mut runtime_min_count: HashMap<String, u64> = HashMap::new();
        let mut runtime_delta: HashMap<String, u64> = HashMap::new();
        // SRD 71: per-cursor narrowed `(start_ord, end_ord)` from the
        // `over` clause's resolved partition. Empty when the cursor
        // wasn't declared with an `over` clause.
        let mut runtime_partition: HashMap<String, (u64, u64)> = HashMap::new();
        // One resolved cursor spec: (name, extent-output wires,
        // extent limit, cursor kind, partition-output wire).
        type CursorSpec = (
            String,
            Option<(String, String)>,
            Option<u64>,
            polydat::iteration::source::CursorKind,
            Option<String>,
        );
        let cursor_specs: Vec<CursorSpec> = kernel
            .program()
            .cursor_schemas()
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.extent_outputs.clone(),
                    s.extent_limit,
                    s.cursor_kind.clone(),
                    s.partition_output.clone(),
                )
            })
            .collect();
        for (name, outputs, limit, cursor_kind, partition_output) in cursor_specs {
            if let Some((start_out, end_out)) = outputs {
                let start = kernel.pull(&start_out).as_u64();
                let end = kernel.pull(&end_out).as_u64();
                let extent = end.saturating_sub(start);
                let final_extent = limit.map(|l| extent.min(l)).unwrap_or(extent);
                runtime_extents.insert(name.clone(), final_extent);
            }
            // SRD 71: resolve the `over` expression now that externs
            // are populated. Accepts a string spec (parsed on the
            // fly), or any Partition / PartitionSpec / PartitionList
            // value flowing through Value::Ext.
            if let Some(out) = &partition_output {
                let value = kernel.pull(out).clone();
                let cursor_extent = runtime_extents.get(&name).copied().unwrap_or_else(|| {
                    kernel
                        .program()
                        .cursor_schemas()
                        .iter()
                        .find(|s| s.name == name)
                        .and_then(|s| s.extent)
                        .unwrap_or(0)
                });
                // Writes one of the `<source>__cursor*` slots by
                // name; looking the slots up by name is the
                // contract from process_cursor.
                let mut write_slot = |suffix: &str, v: polydat::ast::Value| {
                    let slot = format!("{name}__cursor{suffix}");
                    if let Some(idx) = kernel.program().find_input(&slot) {
                        kernel.state().set_input(idx, v);
                    }
                };
                use polydat::ast::Value as PValue;
                let open_extent =
                    !matches!(cursor_kind, polydat::iteration::source::CursorKind::Range,);
                match resolve_over(&value, cursor_extent, open_extent) {
                    Ok(Some(partition)) => {
                        // Narrow the source factory's range using
                        // the partition's bounds.
                        runtime_partition
                            .insert(name.clone(), (partition.start_ord, partition.end_ord));
                        // Write the resolved Partition into the
                        // `<source>__cursor` input slot so downstream
                        // partition-typed nodes (mod_in, cardinality,
                        // etc.) can consume it, and its scalar
                        // projections into the `<source>__cursor__*`
                        // slots that `<source>.cursor.<field>` dotted
                        // access reads (SRD 71 §"Cursor metadata
                        // wires").
                        write_slot("__idx", PValue::U64(partition.idx));
                        write_slot("__partition_count", PValue::U64(partition.count.max(1)));
                        write_slot("__start_pct", PValue::F64(partition.start_pct));
                        write_slot("__end_pct", PValue::F64(partition.end_pct));
                        write_slot("__start_ordinal", PValue::U64(partition.start_ord));
                        write_slot("__end_ordinal", PValue::U64(partition.end_ord));
                        // SRD 71 §"Status / report integration":
                        // the phase-status banner reflects
                        // partition iteration — 1-based index for
                        // display, condensed effective range.
                        // Suppressed for single-partition specs
                        // and non-iterating runs (count <= 1).
                        // Routed through the canonical observer
                        // channel: session.log + stderr + sink.
                        if partition.count > 1 {
                            crate::diag!(
                                crate::observer::LogLevel::Info,
                                "phase '{phase_name}': partition {}/{} [{}..{})",
                                partition.idx + 1,
                                partition.count,
                                partition.start_ord,
                                partition.end_ord
                            );
                        }
                        write_slot("", PValue::from_partition(partition));
                    }
                    Ok(None) => {
                        // Value::None — over expression evaluated to
                        // nothing; no narrowing. The scalar slots
                        // still describe the effective (full)
                        // extent so `q.cursor.*` reads stay
                        // truthful: one partition spanning
                        // [0, extent).
                        write_slot("__end_ordinal", PValue::U64(cursor_extent));
                    }
                    Err(e) => {
                        return crate::phase_outcome::Outcome::failed().with_reason(format!(
                            "cursor '{name}': `over` clause failed to resolve: {e}"
                        ));
                    }
                }
            }
            use polydat::iteration::source::CursorKind::*;
            // Each branch pulls only the outputs its policy needs.
            // `delta_output` is optional — when absent, the source
            // factory uses `base` (the initial extent) as the
            // extension step.
            match &cursor_kind {
                Range => {}
                ExtendingTimed {
                    min_ms_output,
                    delta_output,
                } => {
                    runtime_min_ms.insert(name.clone(), kernel.pull(min_ms_output).as_u64());
                    if let Some(d) = delta_output {
                        runtime_delta.insert(name.clone(), kernel.pull(d).as_u64());
                    }
                }
                ExtendingPasses {
                    min_passes_output,
                    delta_output,
                } => {
                    runtime_min_passes
                        .insert(name.clone(), kernel.pull(min_passes_output).as_u64());
                    if let Some(d) = delta_output {
                        runtime_delta.insert(name.clone(), kernel.pull(d).as_u64());
                    }
                }
                ExtendingCount {
                    min_count_output,
                    delta_output,
                } => {
                    runtime_min_count.insert(name.clone(), kernel.pull(min_count_output).as_u64());
                    if let Some(d) = delta_output {
                        runtime_delta.insert(name.clone(), kernel.pull(d).as_u64());
                    }
                }
                ExtendingElapsedAndPasses {
                    min_ms_output,
                    min_passes_output,
                    delta_output,
                }
                | ExtendingElapsedOrPasses {
                    min_ms_output,
                    min_passes_output,
                    delta_output,
                } => {
                    runtime_min_ms.insert(name.clone(), kernel.pull(min_ms_output).as_u64());
                    runtime_min_passes
                        .insert(name.clone(), kernel.pull(min_passes_output).as_u64());
                    if let Some(d) = delta_output {
                        runtime_delta.insert(name.clone(), kernel.pull(d).as_u64());
                    }
                }
            }
        }
        // Wrap in an `Arc<OpBuilder>` so the per-iteration extern
        // values just bound on `kernel.state` ride along to every
        // fiber via `OpBuilder::create_fiber_builder`. Without
        // this the Arc<GkProgram> alone would be iter-invariant
        // and `{table}` / `{profile}` references in op templates
        // would render with default (empty) values.
        //
        // SRD-13d Phase 9 — pull per-op-template kernel programs
        // for this phase from the scope tree (set by the runner's
        // install loop for materialised op-templates) and install
        // them on the OpBuilder. Each fiber instances one
        // PolydatKernel per program at fiber creation time, bound to
        // the main kernel via the canonical
        // `from_program + materialize_wiring_from_outer` recipe (SRD-13c
        // §"Per-Scope Canonical Kernel Cache"). Flattened
        // op-templates produce no entry — their dispensers fall
        // back to the activity-wide kernel via the standard
        // `nearest_materialised` path.
        // SRD-83 — hold the activation scope's CELL VIEW past
        // OpBuilder's consumption of the kernel: stop-condition
        // predicates bind to this (their true native scope, with the
        // cascade's transitive shared-cell carrier), not to the
        // canonical cached kernel, which is compiled standalone and
        // carries NO cells — a predicate reading a root `shared` wire
        // failed to compile against it (the disarmed-backstop
        // regression, 2026-08-06).
        let activation_scope = Arc::new(kernel.cell_scope_snapshot());
        let op_builder = {
            let mut b = OpBuilder::new(kernel);
            if let Some(phase_idx) = ctx.scope_tree.phase_node_by_name(phase_name) {
                let map = ctx.scope_tree.op_template_programs_for_phase(phase_idx);
                if !map.is_empty() {
                    b = b.with_op_template_programs(map);
                }
            }
            Arc::new(b)
        };
        (
            op_builder,
            ops,
            runtime_extents,
            runtime_min_ms,
            runtime_min_passes,
            runtime_min_count,
            runtime_delta,
            runtime_partition,
            activation_scope,
        )
    } else {
        // Workload-kernel fallback: no per-iteration values to
        // inject. Materialize a fresh subscope of the live
        // parent kernel using the workload program — the only
        // sanctioned construction path. Cells flow forward via
        // the cascade.
        let parent = ctx
            .current_parent_kernel
            .as_ref()
            .expect("workload-kernel fallback requires an installed parent kernel");
        let workload_subscope = parent
            .build_subscope(
                polydat::kernel::subcontext::PolydatMatter::builder()
                    .program(ctx.program.clone())
                    .build()
                    .unwrap(),
            )
            .expect("program-form subscope is infallible");
        // Same stop-condition scope hold as the per-iteration branch.
        let activation_scope = Arc::new(workload_subscope.cell_scope_snapshot());
        let mut b = OpBuilder::new(workload_subscope);
        if let Some(phase_idx) = ctx.scope_tree.phase_node_by_name(phase_name) {
            let map = ctx.scope_tree.op_template_programs_for_phase(phase_idx);
            if !map.is_empty() {
                b = b.with_op_template_programs(map);
            }
        }
        (
            Arc::new(b),
            phase.ops.clone(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            activation_scope,
        )
    };

    let op_sequence = OpSequence::from_ops(iter_ops, ctx.seq_type);
    if op_sequence.stanza_length() == 0 {
        crate::diag!(
            crate::observer::LogLevel::Warn,
            "warning: phase '{phase_name}' has no ops, skipping"
        );
        return crate::phase_outcome::Outcome::skipped();
    }

    // Resolve cycles
    let stanza_len = op_sequence.stanza_length() as u64;
    let spec = phase.cycles.as_deref().unwrap_or("");
    let phase_cycles = if spec == "==auto" {
        crate::diag!(
            crate::observer::LogLevel::Debug,
            "  cycles: auto ({stanza_len} ops = {stanza_len} cycles)"
        );
        stanza_len
    } else if spec == "===auto" || spec.is_empty() {
        stanza_len
    } else if let Some(rest) = spec.strip_prefix("==ops:") {
        // Unification — the implicit `main` phase synthesized
        // for inline / blocks-shorthand workloads sets cycles
        // via the `==ops:` token so the legacy
        // `nmbrs run op=... cycles=20` contract (= 20 op
        // iterations total) survives without re-interpreting
        // every phased workload's `cycles:` field. The
        // payload is the resolved op count, parsed as either
        // a plain integer or a `{polydat_expr}` const expression.
        let mut expanded = rest.to_string();
        for (v, val) in &iter_var_values {
            expanded = expanded.replace(&format!("{{{v}}}"), val);
        }
        expanded = crate::runner::expand_workload_params(&expanded, &ctx.workload_params);
        crate::runner::parse_count(&expanded)
            .or_else(|| {
                if expanded.starts_with('{') && expanded.ends_with('}') {
                    let inner = &expanded[1..expanded.len() - 1];
                    polydat::dsl::compile::eval_const_expr(inner)
                        .ok()
                        .map(|v| v.as_u64())
                } else {
                    None
                }
            })
            .unwrap_or(stanza_len)
    } else {
        // Try resolving from kernel
        let mut expanded = spec.to_string();
        for (v, val) in &iter_var_values {
            expanded = expanded.replace(&format!("{{{v}}}"), val);
        }
        expanded = crate::runner::expand_workload_params(&expanded, &ctx.workload_params);
        let stanzas = crate::runner::parse_count(&expanded)
            .or_else(|| {
                if expanded.starts_with('{') && expanded.ends_with('}') {
                    let inner = &expanded[1..expanded.len() - 1];
                    polydat::dsl::compile::eval_const_expr(inner)
                        .ok()
                        .map(|v| v.as_u64())
                } else {
                    None
                }
            })
            .unwrap_or(1);
        stanzas * stanza_len
    };

    // Diagnostic output — value-provenance / wiring view.
    if ctx.diag.show_wiring {
        let note = if is_iter {
            let pairs: Vec<String> = iter_var_values
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!(" ({})", pairs.join(", "))
        } else {
            String::new()
        };
        crate::describe::print_wiring_analysis(phase_name, &note, &iter_op_builder.program());
    }
    // NOTE: the depth==Phase early-return used to live here, but
    // it short-circuited *before* component attach + control
    // declarations, leaving `dryrun=controls` walking an empty
    // tree. The guard now fires below — after phase_component
    // attach, `Activity::attach_component` (declares concurrency
    // / rate), and the per-phase adapter `declare_controls`
    // pass — so the discovery path sees the full control surface
    // without spinning up the fiber pool / progress thread / or
    // running any cycles.

    // Resolve concurrency: `{param}` / `{iter_var}` interpolation,
    // a literal integer, or a BARE NAME resolved scope-aware — an
    // iteration variable, a parent-kernel wire/const (which covers
    // cascaded workload params and scope consts), or a raw workload
    // param, in shadowing order (SRD-16: inner scope wins). "cycle
    // is just a wire name" applies to config scalars too:
    // `concurrency: concurrency` reads the workload's `concurrency`
    // param the same way `batch: stride` reads a scope const.
    let phase_concurrency = match phase.concurrency.as_ref() {
        Some(s) => {
            let mut exp = crate::runner::expand_workload_params(s, &ctx.workload_params);
            for (v, val) in &iter_var_values {
                exp = exp.replace(&format!("{{{v}}}"), val);
            }
            let parsed = exp.parse::<usize>().ok().or_else(|| {
                let bare = exp.trim();
                iter_var_values
                    .get(bare)
                    .cloned()
                    .or_else(|| {
                        ctx.current_parent_kernel
                            .as_ref()
                            .and_then(|k| k.lookup(bare))
                            .map(|v| match v {
                                polydat::ast::Value::U64(n) => n.to_string(),
                                polydat::ast::Value::F64(f) => (f as u64).to_string(),
                                polydat::ast::Value::Str(s) => s.to_string(),
                                other => other.to_display_string(),
                            })
                    })
                    .or_else(|| ctx.workload_params.get(bare).cloned())
                    .and_then(|resolved| resolved.trim().parse::<usize>().ok())
            });
            match parsed {
                Some(v) => v,
                // No `phase 'X':` prefix here — the recording
                // chokepoint and the runner's final error line each
                // add phase context; embedding it doubles it.
                None => {
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "concurrency '{exp}' is neither an integer nor a resolvable \
                     name (checked iteration variables, the parent scope, and \
                     workload params)"
                    ));
                }
            }
        }
        None => ctx.concurrency,
    };

    // Phase labels read straight off the parent kernel's
    // formal scope-coordinate path (SRD 18b §"Scope
    // coordinates" / `polydat::kernel::scope_coords`).
    // The path is leaf-first: each entry is one scope's own
    // coordinates (the LHS of its `var in expr` clauses).
    // We render as striated parens so the operator can read
    // off the active iteration at each enclosing level
    // independently:
    //
    //     ann_query (k=10, limit=20), (table=…, optimize_for=…)
    //
    // Empty strata (scenario lists, no-coord phases) are
    // skipped. With no parent kernel (root-scope phase) the
    // label is empty.
    let phase_labels = match ctx.current_parent_kernel.as_ref() {
        Some(parent) => format_scope_coordinate_path(parent.scope_coordinates()),
        None => String::new(),
    };
    let stanza_len = op_sequence.stanza_length();
    // `phase_starting` moved below the source-factory + progress-
    // extent computation so the cycle count we report is the same
    // value that bounds the loop. See immediately after
    // `progress_extent` is resolved.

    // Activity name carries only the **leaf** scope's iter
    // coords — the innermost stratum from
    // `parent_kernel.scope_coordinates()[0]`. The full path
    // (`(k=10), (table=…), (profile=…)`) is what
    // `phase_labels` carries for diagnostic / identity uses;
    // surfacing the leaf only on the inline status line keeps
    // the line short enough to fit in a typical terminal
    // width and shows the operator the iter that's actively
    // changing across executions of this same phase. Outer
    // strata are stable across an entire scope iteration and
    // are visible via the TUI's tree / pre-map plan.
    let activity_name = {
        let leaf_label = ctx
            .current_parent_kernel
            .as_ref()
            .and_then(|k| k.scope_coordinates().first())
            .filter(|c| !c.is_empty())
            .map(|c| {
                c.vars
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.to_display_string()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if leaf_label.is_empty() {
            phase_name.to_string()
        } else {
            format!("{phase_name} ({leaf_label})")
        }
    };

    // If the compiled kernel declares cursors, create a source factory
    // from the first cursor's schema (name + extent). Otherwise the
    // Activity falls back to a range source named "cycles".
    let source_factory: Option<Arc<dyn polydat::iteration::source::DataSourceFactory>> = {
        let program = iter_op_builder.program();
        let schemas = program.cursor_schemas();
        if let Some(schema) = schemas.first() {
            // Prefer the runtime-resolved extent (computed above after
            // iter-var externs were bound) over the schema's
            // compile-time extent — the latter is None when the
            // cursor's `range(...)` bounds depend on wire-bound
            // externs like `vector_count("{dataset}:{profile}")`.
            let extent = runtime_cursor_extents
                .get(&schema.name)
                .copied()
                .or(schema.extent)
                .unwrap_or(phase_cycles);
            // SRD 71: narrow `[0, extent)` to `[start_ord, end_ord)`
            // when the cursor was declared with `over`. Falls back
            // to the full range when no narrowing applies.
            let partitioned = runtime_cursor_partition.contains_key(&schema.name);
            let (range_start, range_end) = runtime_cursor_partition
                .get(&schema.name)
                .copied()
                .unwrap_or((0, extent));
            let effective_extent = range_end.saturating_sub(range_start);
            use polydat::iteration::source as src;
            // For the extending kinds, the declared `base` (the
            // per-pass chunk size, carried in `extent`) keeps its
            // meaning under partition narrowing: the source walks
            // base-sized chunks within `[start_ord, end_ord)` and
            // the partition end is a hard cap (`bounded`), per
            // SRD 71 §"Interaction with existing cursor surface".
            // A base larger than the partition clamps to it.
            let chunk = extent.min(effective_extent);
            // Effective extension delta — `runtime_cursor_delta`
            // when the workload supplied an explicit `delta` arg,
            // else the (possibly clamped) base chunk.
            let delta = runtime_cursor_delta
                .get(&schema.name)
                .copied()
                .unwrap_or(chunk);
            // Apply the partition cap to an extending factory.
            let bound = |f: src::ExtendingRangeSourceFactory| {
                if partitioned { f.bounded(range_end) } else { f }
            };
            match &schema.cursor_kind {
                src::CursorKind::Range => Some(Arc::new(src::RangeSourceFactory::named(
                    &schema.name,
                    range_start,
                    range_end,
                ))
                    as Arc<dyn src::DataSourceFactory>),
                src::CursorKind::ExtendingTimed { .. } => {
                    let min_ms = runtime_cursor_min_ms
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let policy: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilElapsedPolicy { min_ms, delta });
                    Some(Arc::new(bound(src::ExtendingRangeSourceFactory::new(
                        &schema.name,
                        range_start,
                        chunk,
                        policy,
                    ))) as Arc<dyn src::DataSourceFactory>)
                }
                src::CursorKind::ExtendingPasses { .. } => {
                    let min_passes = runtime_cursor_min_passes
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let policy: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilPassesPolicy { min_passes, delta });
                    Some(Arc::new(bound(src::ExtendingRangeSourceFactory::new(
                        &schema.name,
                        range_start,
                        chunk,
                        policy,
                    ))) as Arc<dyn src::DataSourceFactory>)
                }
                src::CursorKind::ExtendingCount { .. } => {
                    let min_count = runtime_cursor_min_count
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let policy: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilCountPolicy { min_count, delta });
                    Some(Arc::new(bound(src::ExtendingRangeSourceFactory::new(
                        &schema.name,
                        range_start,
                        chunk,
                        policy,
                    ))) as Arc<dyn src::DataSourceFactory>)
                }
                src::CursorKind::ExtendingElapsedAndPasses { .. } => {
                    let min_ms = runtime_cursor_min_ms
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let min_passes = runtime_cursor_min_passes
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let elapsed: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilElapsedPolicy { min_ms, delta });
                    let passes: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilPassesPolicy { min_passes, delta });
                    let policy: Arc<dyn src::ExtensionPolicy> = Arc::new(src::AndPolicy {
                        policies: vec![elapsed, passes],
                    });
                    Some(Arc::new(bound(src::ExtendingRangeSourceFactory::new(
                        &schema.name,
                        range_start,
                        chunk,
                        policy,
                    ))) as Arc<dyn src::DataSourceFactory>)
                }
                src::CursorKind::ExtendingElapsedOrPasses { .. } => {
                    let min_ms = runtime_cursor_min_ms
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let min_passes = runtime_cursor_min_passes
                        .get(&schema.name)
                        .copied()
                        .unwrap_or(0);
                    let elapsed: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilElapsedPolicy { min_ms, delta });
                    let passes: Arc<dyn src::ExtensionPolicy> =
                        Arc::new(src::UntilPassesPolicy { min_passes, delta });
                    let policy: Arc<dyn src::ExtensionPolicy> = Arc::new(src::OrPolicy {
                        policies: vec![elapsed, passes],
                    });
                    Some(Arc::new(bound(src::ExtendingRangeSourceFactory::new(
                        &schema.name,
                        range_start,
                        chunk,
                        policy,
                    ))) as Arc<dyn src::DataSourceFactory>)
                }
            }
        } else {
            None
        }
    };

    // Capture progress info before source_factory is moved into config
    let progress_extent = source_factory
        .as_ref()
        .and_then(|f| f.global_extent())
        .unwrap_or(phase_cycles);
    let progress_cursor_name = source_factory
        .as_ref()
        .map(|f| f.schema().name.clone())
        .unwrap_or_else(|| "cycles".into());
    let progress_fibers = phase_concurrency;
    // SRD-100 P2 — a clone of the source factory that survives the move
    // into `config` below, so the progress thread can re-read the LIVE
    // extent each tick. ExtendingRangeSourceFactory grows its `end` at
    // runtime under `until_elapsed`; `progress_extent` is a one-shot
    // snapshot, so without this the displayed total / percent / ETA would
    // stay pinned at the initial base value (the status line is now folded
    // from `ActivePhase.cursor_extent`, fed by these progress updates).
    let progress_source_factory = source_factory.clone();
    // Read the current (possibly grown) extent from a source-factory clone,
    // falling back to the one-shot snapshot for sourceless / fixed phases.
    fn live_extent_of(
        factory: &Option<Arc<dyn polydat::iteration::source::DataSourceFactory>>,
        fallback: u64,
    ) -> u64 {
        factory
            .as_ref()
            .and_then(|f| f.global_extent())
            .unwrap_or(fallback)
    }

    // Row-level cursor progress for a DATA-DRIVEN phase — returns
    // `(consumed, extent)` straight from the source factory. `(0, 0)`
    // for sourceless phases (plain `cycles:`, where `config.source_factory`
    // is `None`): `rows:` has no meaning there since ops advance the
    // cycle counter one-for-one, so the display keeps the op-denominated
    // `cycles:` chip. When a real cursor is declared the numerator is
    // ordinals consumed (`global_consumed()`) and the denominator the
    // (possibly grown) extent, so the fraction is row-denominated and
    // agrees with the rows/s rate — unlike `cycles:{ops}/{cursor_extent}`
    // which mixes op-count against ordinals when one op strides N rows.
    fn live_rows_of(
        factory: &Option<Arc<dyn polydat::iteration::source::DataSourceFactory>>,
    ) -> (u64, u64) {
        match factory {
            Some(f) => (f.global_consumed(), f.global_extent().unwrap_or(0)),
            None => (0, 0),
        }
    }

    // Stride-alignment warning: when stanza_len doesn't evenly
    // divide the cursor's extent, the boundary stanza is
    // partial (shorter than stanza_len) — which is fine for
    // the source dispatcher itself, but it surprises
    // workload authors whose op-sequence assumes full stanzas
    // (e.g. relevancy evaluators that batch over a
    // stanza-sized window). Same applies per-segment for
    // extending cursors: the `base` step that drives each
    // extension behaves the same way as the initial extent.
    //
    // Fired here, once at phase setup, after both numbers
    // are known. Silent when alignment is clean — the
    // common case for `cursor q = range(0, N)` with `N`
    // divisible by stanza_len.
    let stanza_len_u64 = stanza_len as u64;
    if stanza_len_u64 > 0 && progress_extent > 0 && !progress_extent.is_multiple_of(stanza_len_u64)
    {
        let remainder = progress_extent % stanza_len_u64;
        let full_stanzas = progress_extent / stanza_len_u64;
        crate::diag!(
            crate::observer::LogLevel::Warn,
            "phase '{phase_name}': cursor extent ({progress_extent}) is not an \
             even multiple of stanza length ({stanza_len_u64}) — boundary stanza \
             will be {remainder}/{stanza_len_u64} of a full stride after \
             {full_stanzas} clean stanza(s). If the op-sequence assumes \
             complete stanzas (e.g. for relevancy or aggregation evaluation), \
             align by sizing the cursor to a multiple of {stanza_len_u64}, or \
             by adjusting stanza_concurrency / ops."
        );
    }

    // Now that the source factory and its global extent are
    // settled, fire phase_starting with the actual loop bound —
    // same value that flows into the activity's progress tracker
    // (`cursor_extent: progress_extent` below). Reporting
    // `phase_cycles` here would print `1` for any cursor-driven
    // phase whose `cycles:` field is omitted (the workload's
    // common case), even though the activity goes on to run
    // `vector_count(...)` cycles.
    // Transition the global scene tree first so the
    // observer's startup log line (`LogOnlyObserver`'s
    // `phase_starting`) can look up `[N/total]` and the
    // depth-indent against a Running entry. Reversing this
    // order leaves the per-phase log line un-prefixed and
    // un-indented because the lookup fires before
    // `set_phase_running`.
    crate::scene_tree::with_global_mut(|t| {
        t.set_phase_running_at(scene_node_id, stanza_len);
    });
    ctx.observer.phase_starting(
        scene_node_id,
        phase_name,
        &phase_labels,
        stanza_len,
        progress_extent,
        phase_concurrency,
    );

    // Fire `EventType::PhaseStart` once per phase. No built-in
    // default body — `phase_outcome` already renders the
    // lifecycle bound for the phase's existence, and a
    // separate `▶ starting` line just duplicates the
    // name/coords/seq. Workloads that want a pre-phase
    // header bind `on_phase_start: phase_starting`
    // explicitly.
    {
        let display_labels: String = {
            let parent_coords: Vec<_> = ctx
                .current_parent_kernel
                .as_ref()
                .map(|k| k.scope_coordinates().iter().rev().cloned().collect())
                .unwrap_or_default();
            polydat::kernel::format_scope_coordinate_path(&parent_coords)
        };
        let depth_indent = crate::scene_tree::running_phase_indent();
        let phase_ctx = crate::readout_context::LifecycleContext {
            event: crate::lifecycle::EventType::PhaseStart,
            subject_name: phase_name.to_string(),
            subject_labels: display_labels.clone(),
            depth_indent,
            use_color: crate::observer::use_color(),
            stick_reattached: String::new(),
        };
        crate::readout_context::fire_lifecycle(
            crate::lifecycle::EventType::PhaseStart,
            &ctx.workload_readouts,
            None,
            &phase_ctx,
            Some(&ctx.sqlite_reporter),
        );
    }
    // The phase's provenance identity (SRD-106 D2 + SRD-107 —
    // THE skip-validity anchor), decomposed:
    //
    // - `phase_hash_bytes` — the BASE hash:
    //   [`checkpoint::compose_phase_hash`] over the
    //   ancestor-chain instance hash (immediate parent scopes up
    //   through the workload root; the session-level params
    //   module is EXCLUDED) and the canonical phase-config
    //   digest (ops, bindings, cycles — every declared
    //   `WorkloadPhase` field). Pre-map computable; the resume
    //   planner's candidates use the same formula.
    // - `params_consumed_json` — the per-param leg: which params
    //   this phase's scope actually consumes (GK backward
    //   closure ∪ textual `{name}` scan) mapped to digests of
    //   their CURRENT values. An unrelated param change flips
    //   neither piece; a consumed one flips its digest.
    let phase_config_text = crate::checkpoint::phase_config_canonical_text(&phase);
    let phase_hash_bytes = crate::checkpoint::compose_phase_hash(
        crate::checkpoint::ancestor_chain_hash(&ctx.scope_tree, phase_name),
        crate::checkpoint::config_text_hash(&phase_config_text),
    );
    let phase_hash_hex: String = phase_hash_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let params_consumed_json: Option<String> = {
        let phase_idx = ctx.scope_tree.phase_node_by_name(phase_name);
        let ancestors_below = phase_idx
            .map(|idx| ctx.scope_tree.ancestor_kernels_split(idx).0)
            .unwrap_or_default();
        let op_templates: Vec<std::sync::Arc<polydat::kernel::PolydatProgram>> = phase_idx
            .map(|idx| {
                ctx.scope_tree
                    .op_template_programs_for_phase(idx)
                    .into_values()
                    .collect()
            })
            .unwrap_or_default();
        let consumed = crate::checkpoint::params_scope::consumed_params(
            &iter_op_builder.program(),
            &op_templates,
            &ancestors_below,
            &phase_config_text,
            &ctx.workload_params,
        );
        serde_json::to_string(&consumed).ok()
    };
    if let Some(writer) = ctx.checkpoint_writer.clone() {
        let identity = phase_identity_for(phase_name, &phase_labels);
        writer.update_phase_hash(&identity, phase_hash_bytes, params_consumed_json.clone());

        // Wholesale metrics-purge (SRD-44): on resume, before a
        // phase re-runs, delete every sample_value row from the
        // prior invocation that carries this phase's label set.
        // No-op for fresh runs (is_resume = false) and for
        // freshly-declared phases that never executed before.
        if ctx.resume_plan.is_resume {
            ctx.push_label("phase", phase_name);
            let labels_for_purge = ctx.labels();
            ctx.pop_label();
            let mut guard = ctx
                .sqlite_reporter
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(reporter) = guard.as_mut() {
                let n = reporter.purge_samples_with_labels(&labels_for_purge);
                if n > 0 {
                    crate::diag!(
                        crate::observer::LogLevel::Info,
                        "resume: purged {n} prior sample rows for phase '{phase_name}'"
                    );
                }
            }
        }

        writer.phase_started(&identity);
        if let Err(e) = writer.flush() {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "checkpoint flush at phase '{phase_name}' start: {e}"
            );
        }
    }

    // SRD-77 `--scope=changed` skip-gate: now that the
    // phase_hash is computed, defer to the refine plan. If the
    // plan says this (name, labels, hash) is unchanged from a
    // prior outcome, short-circuit the same shape as the
    // structural-walker missing-skip path. `should_skip` is a
    // no-op for non-Changed scopes and for non-refine runs.
    //
    // SRD-106 additions: an idempotent prereq under scope=missing
    // ALSO lands here (its no-hash fast path is disabled — skip
    // validity requires Completed+Succeeded AND hash equality, so a
    // stale load re-runs); and a phase the `phases=` filter names
    // never skips (selection is intent to run).
    let explicitly_selected = ctx
        .phase_filter
        .as_ref()
        .map(|pat| pat.is_match(phase_name))
        .unwrap_or(false);
    let is_prereq_class = phase
        .checkpoint
        .as_ref()
        .map(|c| c.idempotent)
        .unwrap_or(false);
    if let Some(plan) = ctx.refine_plan.as_ref()
        && !explicitly_selected
        && (plan.scope == crate::refine_plan::RefineScope::Changed
            || (plan.scope == crate::refine_plan::RefineScope::Missing && is_prereq_class))
    {
        match plan.unchanged_verdict(
            phase_name,
            &phase_labels,
            &phase_hash_hex,
            &ctx.workload_params,
        ) {
            Ok(()) => {
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "refine: skipping phase '{phase_name}' [{phase_labels}] \
                     (prior completed outcome, hash unchanged)"
                );
                crate::scene_tree::with_global_mut(|t| {
                    t.set_phase_running_at(scene_node_id, 0);
                    t.set_phase_completed_at(scene_node_id, 0.0);
                });
                ctx.observer
                    .phase_starting(scene_node_id, phase_name, &phase_labels, 0, 0, 0);
                ctx.observer
                    .phase_completed(scene_node_id, phase_name, &phase_labels, 0.0);
                crate::phase_end_triggers::fire_phase_completed(phase_name, &phase_labels, 0.0);
                return crate::phase_outcome::Outcome::skipped();
            }
            // SRD-107 Push 3 — the blocker names WHY the phase
            // re-runs, so an operator reads "param 'dataset'
            // changed" instead of a bare hash mismatch. NoPrior
            // is the ordinary new-phase case — quiet.
            Err(crate::refine_plan::SkipBlocker::NoPrior) => {}
            Err(blocker) => {
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "refine: re-running phase '{phase_name}' \
                     [{phase_labels}] — {blocker}"
                );
            }
        }
    }

    // SRD-83 governance `timeout:` (GAP-12) — resolve `{param}` /
    // iter-var references, parse via the ONE time-value parser, and
    // synthesize the visible timeout guard (the `error_rate_max`
    // desugar precedent). A malformed resolved value fails the phase
    // up-front — a governance bound that can't be enforced must never
    // silently become "no bound".
    let timeout_guard = match phase.timeout.as_deref() {
        None => None,
        Some(raw) => {
            let mut t = crate::runner::expand_workload_params(raw, &ctx.workload_params);
            for (v, val) in &iter_var_values {
                t = t.replace(&format!("{{{v}}}"), val);
            }
            match crate::timeval::parse_time_ms(&t) {
                Ok(ms) => {
                    let guard = crate::stop_conditions::StopConditionDecl::timeout_guard(ms);
                    crate::diag!(
                        crate::observer::LogLevel::Info,
                        "phase '{phase_name}': timeout={t} → stop_when: {} \
                         (synthesized)",
                        guard.when
                    );
                    Some(guard)
                }
                Err(e) => {
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "phase '{phase_name}': `timeout: \"{raw}\"` \
                                 (resolved: \"{t}\"): {e}"
                    ));
                }
            }
        }
    };

    // Phase `rate:` — same resolution discipline as `timeout:`
    // above: `{param}` / iter-var references resolve here, and a
    // malformed resolved value fails the phase up front — a rate
    // that cannot be enforced must never silently become
    // "unrated".
    let phase_rate: Option<f64> = match phase.rate.as_deref() {
        None => None,
        Some(raw) => {
            let mut r = crate::runner::expand_workload_params(raw, &ctx.workload_params);
            for (v, val) in &iter_var_values {
                r = r.replace(&format!("{{{v}}}"), val);
            }
            match r.trim().parse::<f64>() {
                Ok(f) => Some(f),
                Err(_) => {
                    return crate::phase_outcome::Outcome::failed().with_reason(format!(
                        "phase '{phase_name}': `rate: \"{raw}\"` (resolved: \
                         \"{r}\") is not a number of ops/sec"
                    ));
                }
            }
        }
    };

    let config = ActivityConfig {
        name: activity_name,
        cycles: phase_cycles,
        concurrency: phase_concurrency,
        rate: phase_rate.or(ctx.rate),
        sequencer: ctx.seq_type,
        error_spec: phase
            .errors
            .clone()
            .unwrap_or_else(|| ctx.error_spec.clone()),
        error_rate_max: phase.error_rate_max.or(ctx.error_rate_max),
        // SRD-83 §throttle — normalized spec (`true` → defaults,
        // `false` → None); the drain loop builds the governor from it
        // after the component (and its controls) attach.
        throttle: phase.throttle.as_ref().and_then(|t| t.to_spec()),
        // SRD-83 — the declared stop conditions that distribute to THIS
        // phase, gathered structurally from two sources, never inferred
        // from a predicate's content:
        //   1. the phase's OWN `stop_when:` whose `each:` names `self`
        //      (the declaring phase) or `phase`;
        //   2. the WORKLOAD's `stop_when:` whose `each:` names `phase`
        //      (a workload declaration fanned out to every phase shell;
        //      a workload `each: self`/`workload` stays at the workload
        //      shell and is compiled into `ctx.workload_shell` instead).
        // Plus, FIRST: the synthesized error-rate guard when
        // `error_rate_max` is opted in — a first-class visible
        // condition in the same list, announced at INFO the way
        // every synthesized layer is (SRD-82 §"AggregateGuard
        // retired as a default"; no hidden conditions).
        stop_when: phase
            .error_rate_max
            .or(ctx.error_rate_max)
            .map(|max| {
                let guard = crate::stop_conditions::StopConditionDecl::error_rate_guard(max);
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "phase '{phase_name}': error_rate_max={max} → stop_when: {} \
                     (synthesized)",
                    guard.when
                );
                guard
            })
            .into_iter()
            // SRD-83 — the synthesized governance-timeout guard rides
            // the same visible list (computed + logged above).
            .chain(timeout_guard)
            .chain(
                phase
                    .stop_when
                    .iter()
                    .filter(|c| {
                        c.each.iter().any(|l| {
                            matches!(
                                l,
                                nmbrs_workload::model::ScopeLevel::SelfScope
                                    | nmbrs_workload::model::ScopeLevel::Phase
                            )
                        })
                    })
                    .chain(ctx.workload_stop_when.iter().filter(|c| {
                        c.each
                            .iter()
                            .any(|l| matches!(l, nmbrs_workload::model::ScopeLevel::Phase))
                    }))
                    // SRD-83 Part 5 — a phase-level trip defaults to `fail`
                    // (the phase result is suspect once a phase predicate fires).
                    .map(|c| {
                        // SRD-83 — interpolate `{param}` placeholders in the predicate
                        // against workload params (and this phase's iteration vars) so a
                        // breaker threshold can be a modular workload param, e.g.
                        // `error_rate > {error_rate_backstop}`. A stop predicate is a
                        // static config expression; the substituted value compiles as a
                        // literal (stop_when's compiled scope can't resolve a bare
                        // param wire — that's continue_if's scope-walked path).
                        let mut when =
                            crate::runner::expand_workload_params(&c.when, &ctx.workload_params);
                        for (v, val) in &iter_var_values {
                            when = when.replace(&format!("{{{v}}}"), val);
                        }
                        crate::stop_conditions::StopConditionDecl {
                            when,
                            effect: crate::stop_conditions::StopConditionDecl::effect_from_str(
                                c.effect.as_deref(),
                                crate::phase_outcome::Outcome::failed(),
                            ),
                            reason: None,
                            // SRD-83 follow-up — detection stays at the phase (this
                            // gather); `at:` (default = innermost of `per:`/`each:`)
                            // selects where the action lands. No `at:` ⇒ phase.
                            target: resolve_stop_scope(c.at, &c.each),
                            // `action: abort` escalates the halt to cancelling
                            // in-flight ops at the trip site.
                            cancel_ops:
                                crate::stop_conditions::StopConditionDecl::action_cancels_ops(
                                    c.effect.as_deref(),
                                ),
                        }
                    }),
            )
            .collect(),
        // Total-attempts budget inherited by this phase's ops: the phase's
        // own `tries:` wins, else the workload-root `tries` param. `None`
        // when neither is set — an op without its own `tries:` field then
        // runs WITHOUT the tries wrapper (single attempt, SRD-82 Part 3b).
        tries: phase.tries.or(ctx.tries),
        // Phase-level `tries:` map-form backoff (root stays numeric, so no
        // ctx fallback); op-level `tries:` maps and standalone
        // `retry_backoff*` params layer on top of this at wrapper build.
        tries_backoff: phase.tries_backoff.clone(),
        stanza_concurrency: 1,
        source_factory,
        // Same plumbing as the workload-level activity build —
        // see runner.rs. Per-phase activity gets the live
        // suppression flag so a TUI dismissal mid-run resumes
        // status-line emission.
        suppress_status_line: ctx.observer.live_suppress_flag().unwrap_or_else(|| {
            Arc::new(std::sync::atomic::AtomicBool::new(
                ctx.observer.suppresses_stderr(),
            ))
        }),
        status_metrics: phase.status_metrics.clone(),
        // Root-first display labels + pre-map seq for the ✓ DONE
        // summary line. `phase_labels` (above, leaf-first) stays
        // canonical for observer event-matching; this field is
        // display-only — reversed so outer scopes lead, mirroring
        // the per-phase header format the terminal observer used
        // to emit on phase-start.
        phase_labels: {
            let parent_coords: Vec<_> = ctx
                .current_parent_kernel
                .as_ref()
                .map(|k| k.scope_coordinates().iter().rev().cloned().collect())
                .unwrap_or_default();
            polydat::kernel::format_scope_coordinate_path(&parent_coords)
        },
        phase_seq: crate::scene_tree::current().and_then(|t| {
            t.nodes
                .get(scene_node_id)
                .and_then(|n| n.seq)
                .map(|s| (s, t.total_phases()))
        }),
        readouts: ctx.workload_readouts.clone(),
        cli_readout_override: ctx.cli_readout_override.clone(),
        snapshot_writer: Some(ctx.sqlite_reporter.clone()),
        // Session-level dryrun mode (`silent`/`fields`/`json`/`op`/`cycle`)
        // drives the dryrun template-parameter injection inside
        // `run_with_adapters`. There is no adapter substitution
        // anymore; the real adapter constructs in full and only
        // the outbound `execute()` is suppressed by the outermost
        // `DryRunWrapper`.
        dry_run_mode: ctx.dry_run.map(String::from),
        // `dryrun=dispenser` (and any path that reaches
        // `run_phase` with depth < Cycle) wants the full
        // dispenser-construction pipeline to fire but no cycles
        // to run. `run_with_adapters` honors this flag right
        // after the per-template map_op loop and before any
        // fiber-pool spawn.
        stop_after_dispenser_init: ctx.diag.depth < crate::runner::ExecDepth::Cycle,
    };

    let phase_driver_owned = phase.adapter.clone().unwrap_or_else(|| ctx.driver.clone());
    let phase_driver = phase_driver_owned.as_str();
    let mut adapter_names = std::collections::HashSet::new();
    adapter_names.insert(phase_driver.to_string());
    for t in op_sequence.templates() {
        if let Some(a) = t.params.get("adapter").and_then(|v| v.as_str())
            && a != phase_driver
        {
            adapter_names.insert(a.to_string());
        }
    }
    // SRD-35 Push A: every adapter is acquired via the
    // session's resource pool. The legacy
    // `create_adapter(...)` factory is wrapped under
    // `LegacyAdapterResource` (`can_share()=false`,
    // forcing `PerPhase` semantics), so behaviour is
    // byte-identical to today: one fresh adapter per
    // phase, dropped at phase end. The visible difference
    // is the lifecycle event quartet
    // (`resource.{attach,init.*,detach,close.*}`) now
    // landing at every phase boundary, giving operators a
    // consistent debug-level view of what the executor is
    // creating and tearing down. Push B will migrate
    // adapters out of the legacy shim into real
    // `SharedResource` impls with `can_share()=true` so
    // the pool can collapse same-config phases onto a
    // single shared instance.
    let mut adapters: HashMap<String, Arc<dyn DriverAdapter>> = HashMap::new();
    let mut attach_guards: Vec<crate::resource_pool::AttachGuard> = Vec::new();
    let phase_seq_label = format!("{:?}", ctx.label_stack);
    for aname in &adapter_names {
        let aname_owned = aname.clone();
        let merged_params = ctx.merged_params.clone();
        let dry_run = ctx.dry_run;
        let aname_for_factory = aname_owned.clone();

        // Push B: prefer the pool-shared path when the
        // adapter has registered a `SharedDriverRegistration`.
        // The executor resolves the driver name here (no
        // instantiation), looks up the shared registration,
        // and uses it to derive the resource key. Phases
        // whose params produce equal keys share a single
        // `Arc<dyn DriverAdapter>` for the rest of the
        // session — fixing the per-phase open/close storm
        // that motivated SRD-35.
        //
        // Adapters that haven't migrated fall through to the
        // Push A `LegacyAdapterResource` shim under
        // `PerPhase` policy — byte-identical to today.
        let shared_reg = if dry_run.is_none() {
            // Dry-run paths use a synthetic adapter that
            // doesn't have a real driver to resolve; skip
            // the shared lookup and let them ride the
            // legacy path.
            let driver_name = crate::adapter::resolve_driver_name(
                &aname_owned,
                &resolve_selector_param(&aname_owned),
                &merged_params,
            );
            driver_name.and_then(|d| crate::adapter::find_shared_driver(&aname_owned, d))
        } else {
            None
        };

        let (adapter, guard) = if let Some(reg) = shared_reg {
            // Shared path: the registration's
            // `resource_key` declares which params are
            // identity-bearing.
            let key = match (reg.resource_key)(&merged_params) {
                Ok(v) => v,
                Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
            };
            match crate::resource_pool::attach_shared_adapter(
                &ctx.resource_pool,
                &aname_owned,
                phase_name,
                key,
                move || async move {
                    crate::runner::create_adapter(&aname_for_factory, &merged_params).await
                },
            )
            .await
            {
                Ok(v) => v,
                Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
            }
        } else {
            // Legacy path: per-phase key, fresh adapter
            // every phase. Push A behaviour preserved.
            match crate::resource_pool::attach_legacy_adapter(
                &ctx.resource_pool,
                &aname_owned,
                phase_name,
                &[
                    ("__phase", phase_name),
                    ("__phase_seq", phase_seq_label.as_str()),
                ],
                move || async move {
                    crate::runner::create_adapter(&aname_for_factory, &merged_params).await
                },
            )
            .await
            {
                Ok(v) => v,
                Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
            }
        };
        // Insert under the REQUESTED adapter name (what the op
        // asked for via `adapter:` or workload default). Today
        // the adapter's self-reported name matches the requested
        // name; the alias-by-request pattern is retained
        // defensively in case a future adapter renames itself
        // through some transform.
        adapters.insert(aname_owned.clone(), adapter);
        attach_guards.push(guard);
    }

    // Build labels from the component tree. `labels` is the full
    // effective set ({session, exec_id, workload} + for_each + phase)
    // — used below to seed the activity. `phase_own_labels` is the
    // subset OWNED at this tier (for_each + phase only): the phase
    // component must not redeclare `{session, exec_id, workload}`,
    // which the session + execution ancestors already own (SRD-88 §2
    // label-ownership invariant). `attach` recomposes the full
    // effective set from the ancestor chain.
    ctx.push_label("phase", phase_name);
    let labels = ctx.labels();
    let phase_own_labels = ctx.incremental_labels();
    ctx.pop_label();

    // Create phase component and attach under the execution component.
    let phase_component = Arc::new(RwLock::new(Component::new(
        phase_own_labels,
        HashMap::new(),
    )));
    component::attach(&ctx.session_component, &phase_component);

    // SRD 40: resolve hdr.sigdigs via walk-up from the phase's
    // ancestor chain before constructing the activity so the
    // histograms start at the configured precision.
    let sigdigs = nmbrs_metrics::instruments::histogram::resolve_hdr_sigdigs(
        &phase_component.read().unwrap_or_else(|e| e.into_inner()),
    );
    // SRD-82 — bind the phase shell's ErrorPolicy at scope-init by
    // resolving from the inherited policy. Equal config → inherits the
    // parent (shared); an override derives a value-equality-shared
    // instance. Resolved once here; the activity holds it.
    let phase_error_policy =
        ctx.error_policy
            .resolve_child(Some(crate::error_policy::PolicyConfig::new(
                config.error_spec.clone(),
                config.error_rate_max,
            )));
    // SRD-83 — the phase's ACTIVATION scope view (cell cascade
    // attached); stop-condition predicates bind to it — their true
    // native scope. NOT the canonical `cached_kernel`: that one is
    // compiled standalone with no cells, and a predicate reading a
    // root `shared` wire (the windowed-backstop shape) failed to
    // compile against it — logged, swallowed, and the phase ran with
    // no stop conditions armed (the disarmed-backstop regression,
    // 2026-08-06).
    let phase_kernel = Some(activation_scope.clone());
    // SRD-91 — outcome-instrument detail (counter vs timer) comes from
    // the run's effective params (`metrics_detail`), which live on
    // `merged_params` (CLI-overlaid), not the workload-declared-only
    // `workload_params`.
    let metric_detail = crate::activity::metric_detail_from_params(&ctx.merged_params);
    let mut activity = Activity::with_params_and_sigdigs(
        config,
        &labels,
        op_sequence,
        ctx.workload_params.clone(),
        sigdigs,
        phase_error_policy,
        phase_kernel,
        &metric_detail,
    );
    // SRD-82 Part 4 — wire this execution's walk-stop flag so in-flight
    // fibers abort cooperatively when the scenario walk halts (a sibling
    // phase failed, or a stop condition tripped). Per-execution
    // (`ctx.workload_shell` is one shell per `ExecCtx`), so a fault in
    // one execution never aborts another's phases.
    activity.walk_stop = Some(ctx.workload_shell.walk_stop_flag());
    // SRD-82 Part 6 — a daemon phase also carries its group's completion
    // flag (set on `task_ctx` by the scenario shell before spawning it),
    // so its fibers stop when the foreground phases it shadows finish.
    activity.daemon_stop = ctx.daemon_stop.clone();
    // SRD-32a Push 3 — propagate the workload-root
    // wrapper-order override and CLI default-order tiebreaker
    // from the run context.
    activity.set_wrappers_override(ctx.wrappers_override.clone());
    activity.set_wrap_default_order(ctx.wrap_default_order.clone());
    // Wire the phase component back onto the activity so the
    // fiber pool can declare its `concurrency` control here
    // (SRD 23 §"Fiber executor").
    activity.attach_component(phase_component.clone());

    // Mark the phase component Running. Instrument registration on
    // this component already happened inside `attach_component` via
    // `ActivityMetrics::register_on`; no `set_instruments` call here.
    {
        let mut pc = phase_component.write().unwrap_or_else(|e| e.into_inner());
        pc.set_state(ComponentState::Running);
    }

    // Adapter-level dynamic controls (SRD 23). Declared here at
    // phase-attach time so `dryrun=controls` walks a populated
    // component tree before any cycles run. `Activity::run_with_adapters`
    // also calls this — the adapter trait contract requires
    // `declare_controls` to be idempotent so the second call is
    // a no-op.
    crate::activity::declare_adapter_controls(&adapters, &phase_component);

    // SRD-89 — cycle-time `control(...)` reads resolve THIS execution's
    // phase-tier controls (`concurrency`, `rate`, adapter controls — declared
    // just above on `phase_component`) by walking up from the fiber's **current
    // component**: each fiber carries a once-per-phase snapshot of this very
    // `phase_component` (taken in `Activity::run_with_adapters` via
    // `runtime_context::snapshot_controls`, read lock-free). So a same-named
    // control resolves to this execution's OWN instance — uniformly in single-run
    // and concurrent runs, with no shared session-root walk and no cross-talk.
    // Nothing to install here; the snapshot is the resolution path.

    // `dryrun=phase` early-exit. Phase depth is already
    // filtered upstream at the structural gate (the scenario
    // walker only calls `run_phase` for depth >= Dispenser),
    // so this branch is defensive — it fires only when a
    // future caller invokes `run_phase` directly with
    // depth < Dispenser. The pre-existing `dryrun=Op` /
    // `dryrun=Dispenser` exits happen post-`run_with_adapters`
    // below, so dispenser construction can run first.
    if ctx.diag.depth < crate::runner::ExecDepth::Dispenser {
        ctx.observer
            .phase_completed(scene_node_id, phase_name, &phase_labels, 0.0);
        crate::phase_end_triggers::fire_phase_completed(phase_name, &phase_labels, 0.0);
        crate::scene_tree::with_global_mut(|t| {
            t.set_phase_completed_at(scene_node_id, 0.0);
        });
        return crate::phase_outcome::Outcome::skipped();
    }

    // SRD-42 §"now" reads come through `MetricsQuery::now()` which
    // walks the live component tree on demand — no per-phase hook
    // registration needed. The legacy `set_live_source` path is
    // gone as of Phase 7b.

    let validation_frame = activity.validation_frame.clone();
    // Feed the observer with live metrics at 500ms cadence.
    // This populates the TUI's ActivePhase panel.
    let observer_for_progress = ctx.observer.clone();
    let progress_metrics = activity.shared_metrics();
    let progress_start = std::time::Instant::now();
    let progress_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let progress_flag = progress_running.clone();

    // SRD-100 P2 — attach this phase's live render handle to the display
    // fold, once, now that the activity (and its metrics / memo) exist
    // (they did NOT at `phase_starting` — the activity is built well after
    // it). A display surface folds `active_phases` and re-derives each
    // phase's status line off this handle, replacing the retired
    // inline-status producer thread. Attached unconditionally: the handle
    // is just snapshot data, so a sink activated mid-phase (Ctrl-T) still
    // finds it — whether to render is the consumer's per-tick decision.
    {
        let (seq, depth_indent) = crate::readout_context::resolve_phase_coord_by_id(scene_node_id);
        // Resolve the on_update render template (workload binding + CLI
        // override + built-in `phase_status` default) into owned bodies the
        // consumer fires with `&self` — the `!Sync` binder never enters the
        // snapshot.
        let update_bodies = {
            let phase_status_default = {
                let readout = crate::readouts::Registry::lookup("phase_status")
                    .expect("phase_status registered");
                crate::readouts::BakedBody::from_single(readout, crate::readouts::Lod::Labeled)
            };
            match crate::readouts::build_event_binder_with_cli(
                &activity.config.readouts,
                crate::lifecycle::EventType::Update,
                phase_status_default,
                activity.config.cli_readout_override.as_deref(),
            ) {
                Ok(mut binder) => binder.take_bodies(crate::lifecycle::EventType::Update),
                Err(e) => {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "readouts: failed to bind on_update — {e}"
                    );
                    Vec::new()
                }
            }
        };
        observer_for_progress.phase_render_attach(crate::observer::PhaseRenderHandle {
            exec_id: crate::execution_context::current_exec_id(),
            name: phase_name.to_string(),
            labels: phase_labels.clone(),
            activity_name: activity.config.name.clone(),
            metrics: progress_metrics.clone(),
            bodies: std::sync::Arc::new(update_bodies),
            memo: activity.memo.clone(),
            gutter: activity.gutter.clone(),
            status_metrics: activity.config.status_metrics.clone().into(),
            concurrency: activity.config.concurrency,
            seq,
            depth_indent,
        });
    }

    // Send initial progress to set cursor info on the observer
    if observer_for_progress.suppresses_stderr() {
        let (rows_consumed, rows_total) = live_rows_of(&progress_source_factory);
        observer_for_progress.phase_progress(&crate::observer::PhaseProgressUpdate {
            exec_id: crate::execution_context::current_exec_id(),
            name: phase_name.to_string(),
            labels: phase_labels.clone(),
            cursor_name: progress_cursor_name.clone(),
            cursor_extent: live_extent_of(&progress_source_factory, progress_extent),
            daemon: phase.daemon,
            rows_consumed,
            rows_total,
            fibers: progress_fibers,
            ops_started: 0,
            ops_finished: 0,
            ops_ok: 0,
            skips: 0,
            errors: 0,
            retries: 0,
            ops_per_sec: 0.0,
            adapter_counters: Vec::new(),
            rows_per_batch: 0.0,
            relevancy: Vec::new(),
        });
    }

    let _progress_thread = if observer_for_progress.suppresses_stderr() {
        let obs = observer_for_progress.clone();
        let cursor_name_for_thread = progress_cursor_name.clone();
        let fibers_for_thread = progress_fibers;
        let name_for_thread = phase_name.to_string();
        let labels_for_thread = phase_labels.clone();
        // Clone so the post-phase final-progress emission below still
        // has access — the thread needs to own its own Arc handle.
        let progress_metrics = progress_metrics.clone();
        // SRD-100 P2 — clone the source-factory handle so the thread can
        // re-read the live (growing) extent each tick.
        let factory_for_thread = progress_source_factory.clone();
        let daemon_for_thread = phase.daemon;
        Some(std::thread::spawn(move || {
            let progress_cursor_name = cursor_name_for_thread;
            let progress_fibers = fibers_for_thread;
            let phase_name = name_for_thread;
            let phase_labels = labels_for_thread;
            while progress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if !progress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let started = progress_metrics
                    .ops_started
                    .load(std::sync::atomic::Ordering::Relaxed);
                let finished = progress_metrics
                    .ops_finished
                    .load(std::sync::atomic::Ordering::Relaxed);
                let successes = progress_metrics.result_success.count();
                let errors = progress_metrics.errors_total.get();
                let elapsed = progress_start.elapsed().as_secs_f64();
                let ops_per_sec = if elapsed > 0.0 {
                    finished as f64 / elapsed
                } else {
                    0.0
                };

                let adapter_counters: Vec<(String, u64, f64)> = progress_metrics
                    .collect_status_counters()
                    .into_iter()
                    .map(|(name, total)| {
                        let rate = if elapsed > 0.0 {
                            total as f64 / elapsed
                        } else {
                            0.0
                        };
                        (name, total, rate)
                    })
                    .collect();

                let stanzas = progress_metrics.stanzas_total.get();
                let find_counter = |want: &str| {
                    adapter_counters
                        .iter()
                        .find(|(n, _, _)| n == want)
                        .map(|(_, t, _)| *t)
                };
                // True average batch size when the CQL batch dispensers
                // publish `_batch_writes`; attempt-based fallback otherwise.
                let rows_per_batch = crate::readout_context::rows_per_batch(
                    find_counter("rows_inserted"),
                    find_counter("_batch_writes"),
                    stanzas,
                )
                .unwrap_or(0.0);

                let relevancy = progress_metrics.collect_relevancy_live();

                // Re-read each tick: an extending cursor grows its extent
                // and consumed climbs, so `rows:` tracks live progress.
                let (rows_consumed, rows_total) = live_rows_of(&factory_for_thread);

                obs.phase_progress(&crate::observer::PhaseProgressUpdate {
                    exec_id: crate::execution_context::current_exec_id(),
                    name: phase_name.clone(),
                    labels: phase_labels.clone(),
                    cursor_name: progress_cursor_name.clone(),
                    cursor_extent: live_extent_of(&factory_for_thread, progress_extent),
                    daemon: daemon_for_thread,
                    rows_consumed,
                    rows_total,
                    fibers: progress_fibers,
                    ops_started: started,
                    ops_finished: finished,
                    ops_ok: successes,
                    skips: progress_metrics.skips_total.get(),
                    errors,
                    retries: errors.saturating_sub(finished.saturating_sub(successes)),
                    ops_per_sec,
                    adapter_counters,
                    rows_per_batch,
                    relevancy,
                });
            }
        }))
    } else {
        None
    };

    // SRD-75: phase-level poll. When the phase declares
    // `poll:`, attach a `PhasePollContext` to the activity so
    // the fiber loop re-runs the source (predicate-driven
    // wall-clock loop) instead of exiting on first
    // exhaustion. The predicate kernel handle is the phase
    // scope's cached kernel — captures land there via the
    // SharedCell mechanism (SRD-13c §"Implementation:
    // SharedCell-backed input slots" §4 "Write through")
    // through the op-template kernel's `extern` import slots
    // (SRD-67 Rule 1 "shared import → share cell").
    if let Some(poll_spec) = phase.poll.as_ref() {
        // Numeric poll bounds carry `{param}` / iter-var
        // references (the `timeout:` / `rate:` gather
        // discipline); resolve them here and fail the phase up
        // front on a malformed value — a bound that cannot be
        // enforced must never silently become the default.
        let resolve_u64 = |raw: Option<&str>, field: &str, default: u64| -> Result<u64, String> {
            let Some(raw) = raw else { return Ok(default) };
            let mut r = crate::runner::expand_workload_params(raw, &ctx.workload_params);
            for (v, val) in &iter_var_values {
                r = r.replace(&format!("{{{v}}}"), val);
            }
            r.trim().parse::<u64>().map_err(|_| {
                format!(
                    "phase '{phase_name}': poll `{field}: \"{raw}\"` (resolved: \
                 \"{r}\") is not a whole number of milliseconds"
                )
            })
        };
        let resolved = (|| -> Result<(u64, u64, u64), String> {
            Ok((
                resolve_u64(poll_spec.interval_ms.as_deref(), "interval_ms", 1000)?,
                resolve_u64(poll_spec.timeout_ms.as_deref(), "timeout_ms", 300_000)?,
                resolve_u64(
                    poll_spec.max_error_retries.as_deref(),
                    "max_error_retries",
                    0,
                )?,
            ))
        })();
        let (interval_ms, timeout_ms, max_error_retries) = match resolved {
            Ok(v) => v,
            Err(e) => return crate::phase_outcome::Outcome::failed().with_reason(e),
        };
        let interval = std::time::Duration::from_millis(interval_ms);
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let phase_kernel = match ctx
            .scope_tree
            .phase_node_by_name(phase_name)
            .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get().cloned())
        {
            Some(k) => k,
            None => {
                return crate::phase_outcome::Outcome::failed().with_reason(format!(
                    "phase '{phase_name}': SRD-75 phase-poll requires the phase \
                 scope kernel to be installed, but no cached kernel was found. \
                 This is a synthesis bug — every phase with `poll:` should \
                 land via the `Bindings` install spec.",
                ));
            }
        };
        let started_at = std::time::Instant::now();
        // SRD-75 `on_timeout` — `Error` (default) when
        // unset OR the workload-declared string is the
        // literal `"error"`; `Abort` only when the
        // workload-author opted in. The parser already
        // validated the string is one of the closed
        // vocabulary, so a mismatch here is impossible
        // and we map confidently.
        let on_timeout_policy = match poll_spec.on_timeout.as_deref() {
            Some("abort") => crate::activity::PhasePollTimeoutPolicy::Abort,
            _ => crate::activity::PhasePollTimeoutPolicy::Error,
        };
        activity.phase_poll = Some(crate::activity::PhasePollContext {
            kernel: phase_kernel,
            interval,
            deadline: started_at + timeout,
            started_at,
            metric_name: poll_spec.metric_name.clone(),
            max_error_retries: max_error_retries.min(u32::MAX as u64) as u32,
            on_timeout: on_timeout_policy,
            // SRD-75 (C5) — strict-gate selectors. The resolution
            // probe reads the SAME framed session-lifetime view the
            // `until:` predicate's `metric()` reader consumes, so
            // "resolvable" means "visible to that reader" — which
            // requires at least one closed cadence frame. Grace =
            // one smallest-cadence frame plus one poll interval from
            // loop start (covers frame close + late registration by
            // a spinning-up producer).
            require: poll_spec.require.clone(),
            require_grace: started_at
                + ctx.cadence_reporter.declared_cadences().smallest()
                + interval,
        });
    }

    crate::diag!(
        crate::observer::LogLevel::Debug,
        "phase '{phase_name}': activity starting (concurrency={phase_concurrency})"
    );
    // Clone the stop-reason handle BEFORE consuming the activity;
    // populated by the per-cycle stop trigger inside the run if a
    // `stop` error handler fires. Read after the run to surface
    // the actual triggering error (instead of a bare "stopped by
    // error handler") in the phase-level error.
    let stop_reason = activity.stop_reason.clone();
    // SRD-83 Part 5 — clone the stop-outcome latch alongside it: the
    // first stop-condition trip records its DECLARED two-axis Outcome
    // here (a `stop` effect = Interrupted+Succeeded), and the phase
    // shell adopts it below instead of deriving failure from the bare
    // stop flag.
    let activity_stop_outcome = activity.stop_outcome.clone();
    // SRD-76 — clone the structured-errors handle BEFORE
    // consuming the activity, mirroring the stop_reason
    // pattern above. Drained at phase end into
    // `PhaseOutcome.errors`.
    let activity_phase_errors = activity.phase_errors.clone();

    // SRD-86 §"Settling" — for an optimize phase whose objective reads
    // live metrics, drive a cadence-fed settle detector across the run.
    // It samples the objective off node X's kernel on every cadence
    // pulse, holds the windowed median in a register, and stops the
    // phase early (interrupted = settled / failed = settle timeout) via
    // the activity stop flag. `None` for a non-volatile objective (the
    // one-shot post-completion read is correct there).
    let settle = ctx.optimize_objective.clone().and_then(|obj| {
        let parent = ctx.current_parent_kernel.clone()?;
        let phase_kernel = ctx
            .scope_tree
            .phase_node_by_name(phase_name)
            .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get().cloned())?;
        crate::optimize::settle::start_settle(
            &parent,
            &phase_kernel,
            &obj,
            &ctx.cadence_reporter,
            activity.stop_flag.clone(),
        )
    });

    // SRD-86 §4 Control-class actuation — when `dispatch_optimization`'s Control
    // branch set `ctx.optimize_servo`, run ONE continuous phase and drive the
    // servoing daemon concurrently (it live-retargets the phase's controls per
    // setting). The daemon ends the phase (`stop_flag`) once its budget is spent;
    // `phase_done` lets it bail if the phase ends first. `optimize_objective` is
    // None here, so the per-phase settle above did not fire — the servo owns
    // settling.
    // `servo_completed` — true when the servoing daemon ran its search to
    // completion and ended the phase itself (a CLEAN early stop, like
    // `settle_succeeded`, NOT an error-handler stop). A servoing error leaves it
    // false so the phase fails.
    let mut servo_completed = false;
    let stopped = if let Some(servo_spec) = ctx.optimize_servo.take() {
        let parent = ctx.current_parent_kernel.clone();
        let phase_kernel = ctx
            .scope_tree
            .phase_node_by_name(phase_name)
            .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get().cloned());
        match (parent, phase_kernel) {
            (Some(parent), Some(phase_kernel)) => {
                let stop_flag = activity.stop_flag.clone();
                let reporter = ctx.cadence_reporter.clone();
                let pc = phase_component.clone();
                let phase_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let pd = phase_done.clone();
                let act = async {
                    let s = crate::runner::run_activity_simple(
                        activity,
                        adapters,
                        phase_driver,
                        iter_op_builder,
                    )
                    .await;
                    pd.store(true, std::sync::atomic::Ordering::Relaxed);
                    s
                };
                let servoed = crate::optimize::servo::servo(
                    servo_spec,
                    stop_flag,
                    reporter,
                    parent,
                    phase_kernel,
                    pc,
                    phase_done,
                );
                let (stopped, servo_res) = tokio::join!(act, servoed);
                match servo_res {
                    Ok(()) => servo_completed = true,
                    Err(e) => crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "phase '{phase_name}': optimizer servoing error: {e}"
                    ),
                }
                stopped
            }
            _ => {
                crate::runner::run_activity_simple(
                    activity,
                    adapters,
                    phase_driver,
                    iter_op_builder,
                )
                .await
            }
        }
    } else {
        crate::runner::run_activity_simple(activity, adapters, phase_driver, iter_op_builder).await
    };
    crate::diag!(
        crate::observer::LogLevel::Debug,
        "phase '{phase_name}': activity returned (stopped={stopped})"
    );

    // A settled phase stopped early with a trustworthy register
    // (Interrupted+Succeeded); a settle timeout (Interrupted+Failed)
    // leaves the phase failed. Tear down the subscription — the
    // register + outcome cell stay readable.
    let settle_succeeded = settle.as_ref().is_some_and(|h| {
        matches!(
            &**h.outcome.load(),   // match by ref — Outcome no longer Copy (SRD-92 reason field)
            Some(o) if o.validity == crate::phase_outcome::Validity::Succeeded
        )
    });
    if let Some(h) = &settle {
        ctx.cadence_reporter.unsubscribe(h.subscriber);
    }

    // Stop progress thread
    progress_running.store(false, std::sync::atomic::Ordering::Relaxed);

    // `dryrun=dispenser` (and any depth < Cycle that reached
    // this far) exit: `run_with_adapters` honored its
    // `stop_after_dispenser_init` flag, the per-template
    // dispensers are constructed, no cycles ran. Skip the
    // cycle-cleanup machinery below (phase poll teardown,
    // checkpoint writer phase row, success/fail PhaseOutcome
    // construction) and fire the sentinel phase_completed
    // directly — the post-run summary shows `[ok]` with no
    // duration suffix, matching the dryrun=phase path.
    if ctx.diag.depth < crate::runner::ExecDepth::Cycle {
        ctx.observer
            .phase_completed(scene_node_id, phase_name, &phase_labels, 0.0);
        crate::phase_end_triggers::fire_phase_completed(phase_name, &phase_labels, 0.0);
        crate::scene_tree::with_global_mut(|t| {
            t.set_phase_completed_at(scene_node_id, 0.0);
        });
        return crate::phase_outcome::Outcome::skipped();
    }

    // Emit one final phase_progress with fresh numbers before
    // `phase_completed`. Short phases (e.g. 100ms ann_query) can
    // finish between progress-thread ticks (every 500ms), so
    // relevancy / counter snapshots that were being updated live
    // would otherwise arrive empty at the observer's summary-
    // capture step. This guarantees the final frame is never stale.
    if ctx.observer.suppresses_stderr() {
        let started_total = progress_metrics
            .ops_started
            .load(std::sync::atomic::Ordering::Relaxed);
        let finished_total = progress_metrics
            .ops_finished
            .load(std::sync::atomic::Ordering::Relaxed);
        let successes = progress_metrics.result_success.count();
        let errors = progress_metrics.errors_total.get();
        let elapsed = progress_start.elapsed().as_secs_f64();
        let ops_per_sec = if elapsed > 0.0 {
            finished_total as f64 / elapsed
        } else {
            0.0
        };
        let adapter_counters: Vec<(String, u64, f64)> = progress_metrics
            .collect_status_counters()
            .into_iter()
            .map(|(name, total)| {
                let rate = if elapsed > 0.0 {
                    total as f64 / elapsed
                } else {
                    0.0
                };
                (name, total, rate)
            })
            .collect();
        let stanzas = progress_metrics.stanzas_total.get();
        let find_counter = |want: &str| {
            adapter_counters
                .iter()
                .find(|(n, _, _)| n == want)
                .map(|(_, t, _)| *t)
        };
        // True average batch size when the CQL batch dispensers publish
        // `_batch_writes`; attempt-based fallback otherwise.
        let rows_per_batch = crate::readout_context::rows_per_batch(
            find_counter("rows_inserted"),
            find_counter("_batch_writes"),
            stanzas,
        )
        .unwrap_or(0.0);
        let relevancy = progress_metrics.collect_relevancy_live();
        let (rows_consumed, rows_total) = live_rows_of(&progress_source_factory);
        ctx.observer
            .phase_progress(&crate::observer::PhaseProgressUpdate {
                exec_id: crate::execution_context::current_exec_id(),
                name: phase_name.to_string(),
                labels: phase_labels.clone(),
                cursor_name: progress_cursor_name.clone(),
                cursor_extent: live_extent_of(&progress_source_factory, progress_extent),
                daemon: phase.daemon,
                rows_consumed,
                rows_total,
                fibers: progress_fibers,
                ops_started: started_total,
                ops_finished: finished_total,
                ops_ok: successes,
                skips: progress_metrics.skips_total.get(),
                errors,
                retries: errors.saturating_sub(finished_total.saturating_sub(successes)),
                ops_per_sec,
                adapter_counters,
                rows_per_batch,
                relevancy,
            });
    }

    // SRD-83 Part 5 — a graceful `stop`-effect trip is a CLEAN early
    // halt (Interrupted + Succeeded): the phase keeps its partial
    // result. The latch is written only by the stop-condition trip
    // site, and only when that trip also won the `stop_reason` slot —
    // so the first stopper owns both the reason and the outcome. Every
    // other stop source (error router, walk stop, poll timeout,
    // Ctrl-C) leaves it None and keeps failure semantics.
    let graceful_stop = stopped
        && activity_stop_outcome
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .is_some_and(|o| !o.is_failure());

    // Phase-level `metrics:` emission. Pull each `__metric_<name>`
    // from the phase scope kernel (with the executor-injected
    // `phase_start` set to this phase's chronological start) and
    // record it on the phase component as the declared instrument.
    // Done BEFORE the final cadence flush below so the recorded
    // values land in this phase's terminal window. Emitted when the
    // outcome's VALIDITY is Succeeded — natural completion or a
    // graceful `stop`-effect halt (SRD-83: the partial result is a
    // valid measurement); a failed/error-stopped phase's metrics
    // would be misleading and are suppressed.
    if (!stopped || graceful_stop) && !phase.metrics.is_empty() {
        let phase_start_epoch_ms = (phase_start_nanos / 1_000_000) as u64;
        if let Err(e) = emit_phase_metrics(
            &ctx.scope_tree,
            phase_name,
            &phase,
            phase_start_epoch_ms,
            &phase_component,
        ) {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "phase '{phase_name}': phase-metric emission: {e}"
            );
        }
    }

    // SRD-86 A10 — for an optimize-node search, read the objective wire
    // POST-EXECUTION (here, after `run_activity_simple`), so it observes the
    // run's produced state — never at setup. Node X's kernel is its program
    // bound to its live parent scope (which carries the per-eval coordinate), so
    // the read rebuilds exactly that. This is the coordinate-function path —
    // correct for objectives that are a deterministic function of the
    // coordinate. A volatile objective over a windowed metric is settled across
    // the run by the cadence-fed detector instead (`settle` above, SRD-86
    // §"Settling via the cadence pulse"): its register holds the windowed,
    // coordinate-scoped value, which we read here in preference to the one-shot
    // pull (the trailing window a one-shot read would see is empty).
    ctx.optimize_objective_value = None;
    if let Some(h) = &settle {
        // Volatile objective: the one-shot read cannot capture it. Use
        // the windowed value settled (or last-smoothed) across the run.
        ctx.optimize_objective_value = Some(h.register.load().value);
    } else if !stopped
        && let (Some(obj), Some(parent), Some(phase_kernel)) = (
            ctx.optimize_objective.clone(),
            ctx.current_parent_kernel.clone(),
            ctx.scope_tree
                .phase_node_by_name(phase_name)
                .and_then(|idx| ctx.scope_tree.nodes[idx].cached_kernel.get().cloned()),
        )
    {
        ctx.optimize_objective_value = read_objective_at_completion(&parent, &phase_kernel, &obj);
    }

    // Lifecycle flush: capture final delta, route through the
    // cadence reporter (single writer of windowed snapshots), and
    // deliver to the scheduler-tree reporters for external sinks.
    //
    // The call to `close_path` at the end is the primary lifecycle
    // boundary for this phase's metrics. By the time the ingests
    // above return, no more data for this phase's label set will
    // ever arrive — the next for_each iteration (or the next phase)
    // uses a different label combination, so it lands in a different
    // path. Closing here publishes the phase's windows now instead
    // of leaving them to idle until the next cadence tick (which
    // could be 30s away) or session shutdown (which produced a
    // thundering herd of stale windows).
    {
        let mut final_delta = phase_component
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .capture_delta_auto(std::time::Duration::from_secs(1));
        // SRD-40b §11 / SRD-42 §"Component lifecycle: scope_close
        // flush" — this delta is the phase's last-tick contribution
        // before teardown. Mark it partial so the cadence stream
        // can distinguish it from naturally pulse-flushed windows.
        //
        // The `_auto` variant computes the interval from real
        // wall-clock elapsed time since the previous capture
        // (typically the last scheduler tick), rather than
        // stamping a nominal 1-second tail. Eliminates the
        // 1-second quantization that previously made e.g.
        // four phases of differing real durations all report
        // `cycles_total_rate = 1250` (10000 ops / 8 ticks).
        // The `Duration::from_secs(1)` argument is only the
        // fallback for the edge case of a phase ending before
        // any scheduler tick captured.
        final_delta.mark_partial();
        ctx.cadence_reporter.ingest(&labels, final_delta.clone());
        ctx.stop_handle.report_frame(&final_delta);

        // Flush validation metrics (recall, precision) as gauges
        if let Some(mut vframe) = validation_frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            // Same scope_close partial annotation — vframe is the
            // phase's terminal validation snapshot.
            vframe.mark_partial();
            // Generic observability point: a validation MetricSet
            // is being handed to the cadence reporter. Carries
            // the phase labels (whatever the scope tree pushed)
            // and the family count — applies to ANY workload
            // that produces a validation frame.
            if crate::observer::trace_enabled() {
                crate::observer::trace(
                    &labels,
                    &format!(
                        "event=validation_frame.ingest family_count={}",
                        vframe.len()
                    ),
                );
            }
            ctx.cadence_reporter.ingest(&labels, vframe.clone());
            ctx.stop_handle.report_frame(&vframe);
        }

        ctx.cadence_reporter.close_path(&labels);
    }

    // Transition to Stopped
    {
        let mut pc = phase_component.write().unwrap_or_else(|e| e.into_inner());
        pc.set_state(ComponentState::Stopped);
    }

    let phase_duration = phase_start.elapsed().as_secs_f64();
    // SRD-83 — this child phase's op / error totals, folded into the
    // workload shell's aggregate at each outcome path below. `cycles`
    // is ops dispatched; the error count is the per-OP terminal failure
    // tally (`result_failure`, SRD-91) so the workload-shell error rate
    // stays a per-op proportion in [0,1] (not the per-attempt
    // `errors_total`, which counts retries).
    let phase_op_count = progress_metrics.cycles_completed();
    let phase_error_count = progress_metrics.result_failure.count();
    if stopped && !settle_succeeded && !servo_completed && !graceful_stop {
        // Pull the first triggering error captured by the
        // activity's stop_flag setter (activity.rs per-cycle
        // dispatch). Fall back to a bare reason when the stop
        // came from an early-init path that doesn't populate it
        // (validate_bind_points, missing adapter, etc. — those
        // paths log directly to stderr).
        let reason = stop_reason
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "stopped by error handler".to_string());
        let detail_msg = format!("stopped by error handler: {reason}");
        // Indents to phase scope depth — same hierarchic
        // pattern as the completion line below. Red on the
        // phase name + the failure-reason summary so a failure
        // is visually distinct from a normal completion in
        // tui=terminal output.
        let depth_indent = crate::scene_tree::running_phase_indent();
        let color = crate::observer::use_color();
        let bold = if color { "\x1b[1m" } else { "" };
        let red = if color { "\x1b[31m" } else { "" };
        let dim = if color { "\x1b[2m" } else { "" };
        let reset = if color { "\x1b[0m" } else { "" };
        // SRD-? — include the phase's striated scope-coord
        // chain on the error line so a sweep-cell failure
        // points at the specific cell. Same change-only
        // summary lens as `phase_outcome::render_labeled_value`
        // — completed-phase event whether it succeeded or
        // failed; the scope-open lines above the error already
        // establish the unchanged context, so only the axes
        // that took a new value here should appear.
        let error_head_consumed: usize = depth_indent.chars().count()
            + "phase '".chars().count()
            + phase_name.chars().count()
            + "' ".chars().count();
        let coords_part = crate::readouts::builtins::phase_outcome::format_coords_block(
            &phase_labels,
            color,
            error_head_consumed,
            &format!("{depth_indent}    "),
            /* summarize_changed_only */ true,
        );
        crate::diag!(
            crate::observer::LogLevel::Error,
            "{depth_indent}phase '{bold}{phase_name}{reset}'{coords_part} {red}{detail_msg}{reset} {dim}({phase_duration:.2}s){reset}"
        );
        ctx.observer
            .phase_failed(scene_node_id, phase_name, &phase_labels, &detail_msg);
        crate::phase_end_triggers::fire_phase_failed(phase_name, &phase_labels, &detail_msg);
        // SRD-76 — build the structured PhaseOutcome and
        // install it on the scene tree alongside the
        // legacy `set_phase_failed` mirror. The
        // `phase_errors` buffer carries any per-cycle or
        // phase-level errors captured during execution
        // (today: the SRD-75 poll_timeout path; future
        // pushes extend to per-cycle dispatch failures).
        // If the buffer is empty (rare race), synthesize a
        // single entry from `detail_msg` so the
        // structural invariant "Failed ⇒ at least one
        // error" holds without a debug-assert blowup.
        let errors = activity_phase_errors
            .lock()
            .ok()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let errors = if errors.is_empty() {
            vec![crate::phase_outcome::PhaseErrorDetail {
                class: "phase_failed".into(),
                message: reason.clone(),
                op_name: None,
                cycle: None,
                op_template: None,
                op_resolved: None,
                at_nanos: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
                retryable: false,
            }]
        } else {
            errors
        };
        let outcome = crate::phase_outcome::PhaseOutcome::failed(
            crate::phase_outcome::PhaseIdentity::new(phase_name, phase_labels.as_str()),
            phase_duration,
            errors,
        )
        .with_phase_hash(phase_hash_hex.clone())
        .with_params_consumed(params_consumed_json.clone());
        // SRD-76 Push 3 — persist before installing on the
        // scene tree so a panic during scene-tree
        // mutation still leaves a durable row on disk. The
        // write itself is best-effort: a sqlite failure
        // logs at Warn and doesn't propagate (the in-memory
        // scene tree remains the canonical state).
        if let Ok(mut guard) = ctx.sqlite_reporter.lock()
            && let Some(reporter) = guard.as_mut()
        {
            let row = outcome.to_sqlite_row(&ctx.session_id, ctx.exec_id, phase_start_nanos);
            reporter.write_phase_outcome(&row);
        }
        crate::scene_tree::with_global_mut(|t| {
            t.set_phase_failed_at(scene_node_id, &detail_msg);
            t.set_phase_outcome_at(scene_node_id, outcome);
        });
        if let Some(writer) = ctx.checkpoint_writer.as_ref() {
            let identity = phase_identity_for(phase_name, &phase_labels);
            writer.phase_failed(&identity, &detail_msg);
            if let Err(e) = writer.flush() {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "checkpoint flush after phase '{phase_name}' failed: {e}"
                );
            }
        }
        // SRD-82 Part 4/6 — fold this failed child into the workload
        // shell. The default scenario-graph `children_failed > 0` rule
        // (a `fail` effect) trips here; the walk-stop it latches halts
        // sibling phases the local `Err` cascade can't reach (concurrent
        // / cross-subtree). Route by the tripping outcome's cause: a
        // `fail` effect is a FAULT stop (the session is failing — this
        // phase returns `Err` below, driving the non-zero exit), so the
        // skipped tail is recorded as a fault, not mistaken for a
        // graceful early stop.
        if let Some((outcome, reason)) =
            ctx.workload_shell
                .record_phase(true, phase_op_count, phase_error_count)
        {
            let cause = if outcome.is_failure() {
                crate::session_signals::StopCause::Fault
            } else {
                crate::session_signals::StopCause::Interrupt
            };
            crate::session_signals::request_shell_stop(cause);
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "scenario stop-on-error ({reason}) after phase \
                 '{phase_name}' — halting remaining walk"
            );
        }
        return crate::phase_outcome::Outcome::failed()
            .with_reason(format!("phase '{phase_name}' {detail_msg}"));
    }

    // Indent the completion line by scope depth so
    // No `phase 'X' complete (Ns)` log line — the activity-level
    // DONE summary (✓ + stats line in `activity.rs`) is the single
    // canonical completion marker. Emitting both produced
    // duplicate end-of-phase output; the activity line carries
    // the throughput / ok-rate / errors detail the user needs,
    // while the phase identity and coords are already on the
    // phase-starting row directly above.
    ctx.observer
        .phase_completed(scene_node_id, phase_name, &phase_labels, phase_duration);
    // Phase-end trigger fan-out: registered triggers
    // (plot re-render, report rebuild, etc.) run on the
    // worker thread so the executor's loop isn't blocked.
    crate::phase_end_triggers::fire_phase_completed(phase_name, &phase_labels, phase_duration);
    // SRD-76 — install the structured success outcome.
    // Drain any residual entries from the phase_errors
    // buffer just in case (a non-stopping retryable
    // failure may have been logged but not promoted to a
    // stop_flag); they ride along as non-fatal context.
    let success_errors = activity_phase_errors
        .lock()
        .ok()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default();
    let success_outcome = crate::phase_outcome::PhaseOutcome {
        phase_id: crate::phase_outcome::PhaseIdentity::new(phase_name, phase_labels.as_str()),
        // SRD-83 Part 5 — a graceful `stop`-effect trip records the
        // condition's declared axes: Interrupted (the extent was cut
        // short on purpose) + Succeeded (the partial result stands).
        // Natural completion — and the settle/servo early-finish paths,
        // which reached their own goal — stay Completed.
        disposition: if graceful_stop {
            crate::phase_outcome::Disposition::Interrupted
        } else {
            crate::phase_outcome::Disposition::Completed
        },
        validity: crate::phase_outcome::Validity::Succeeded,
        duration_secs: phase_duration,
        errors: success_errors,
        resume_cursor: None,
        phase_hash: Some(phase_hash_hex.clone()),
        params_consumed: params_consumed_json.clone(),
    };
    // SRD-76 Push 3 — persist the success outcome. Same
    // best-effort policy as the failure path above; the
    // scene tree is the canonical in-memory state.
    if let Ok(mut guard) = ctx.sqlite_reporter.lock()
        && let Some(reporter) = guard.as_mut()
    {
        let row = success_outcome.to_sqlite_row(&ctx.session_id, ctx.exec_id, phase_start_nanos);
        reporter.write_phase_outcome(&row);
    }
    crate::scene_tree::with_global_mut(|t| {
        t.set_phase_completed_at(scene_node_id, phase_duration);
        t.set_phase_outcome_at(scene_node_id, success_outcome);
    });
    if let Some(writer) = ctx.checkpoint_writer.as_ref() {
        let identity = phase_identity_for(phase_name, &phase_labels);
        writer.phase_completed(&identity, phase_duration);
        if let Err(e) = writer.flush() {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "checkpoint flush after phase '{phase_name}' completed: {e}"
            );
        }
    }
    // SRD-83 — fold this completed child into the workload shell. A
    // workload-level stop condition (e.g. `op_count > N`,
    // `children_done >= K`) may trip on the new aggregate even though
    // this phase succeeded; the latch halts the remaining walk. This
    // is a GRACEFUL stop — nothing failed — so flag it so the
    // end-of-run unreached-phase check treats the deliberately-skipped
    // tail like a Ctrl-C stop (no "phases were not executed" warning,
    // clean exit) rather than as stranded-by-failure.
    if let Some((outcome, reason)) =
        ctx.workload_shell
            .record_phase(false, phase_op_count, phase_error_count)
    {
        // The ACTUAL aggregate wires that crossed the threshold
        // (`children_done=2/3, …`), so the message says WHY, not just which
        // predicate. Snapshotted right after the tripping `record_phase`.
        let actual = ctx.workload_shell.describe_state();
        if outcome.is_failure() {
            // SRD-83 Part 5 — a `fail`-effect workload condition tripped
            // on the new aggregate even though this phase succeeded
            // (e.g. an aggregate error-rate breach). The run is a
            // failure: return Err so the session exits non-zero and the
            // walk halts. The phase's own outcome stays Completed (it
            // did complete); the workload-level breach is what fails.
            crate::diag!(
                crate::observer::LogLevel::Error,
                "workload stop condition tripped ({reason}) — actual: {actual} \
                 — after phase '{phase_name}' — failing session"
            );
            return crate::phase_outcome::Outcome::failed().with_reason(format!(
                "workload stop condition tripped: {reason} — actual: {actual}"
            ));
        }
        // A graceful `stop` effect: nothing failed, halt the walk and
        // flag the deliberately-skipped tail so the unreached-phase
        // check treats it like a clean Ctrl-C stop.
        crate::session_signals::request_graceful_stop();
        crate::diag!(
            crate::observer::LogLevel::Warn,
            "workload stop condition tripped ({reason}) — actual: {actual} \
             — after phase '{phase_name}' — halting remaining walk"
        );
    }
    if graceful_stop {
        // SRD-83 Part 5 — surface the trip's declared graceful outcome
        // (Interrupted + Succeeded) to the walk, carrying the trip
        // reason so the scenario fold and readouts can say why the
        // extent was cut short.
        let mut oc = crate::phase_outcome::Outcome::interrupted();
        if let Some(r) = stop_reason.lock().ok().and_then(|g| g.clone()) {
            oc = oc.with_reason(r);
        }
        return oc;
    }
    crate::phase_outcome::Outcome::completed()
}

/// Phase-level `metrics:` emission, run once at phase completion.
///
/// Builds a fresh subscope from the phase scope kernel (so the pull
/// has its own evaluation state), sets the executor-injected
/// `phase_start` origin wire to the phase's chronological start
/// (epoch millis), then pulls each `__metric_<name>` the phase
/// synthesiser emitted and records it on the phase component as the
/// declared instrument (gauge by default). This mirrors the per-cycle
/// op-metric path (`wrappers::metrics`) but fires exactly once: the
/// value expression (e.g. `current_epoch_millis() - phase_start`)
/// reads the clock at this moment, yielding the phase's wall-clock
/// duration.
fn emit_phase_metrics(
    scope_tree: &crate::scope_tree::ScopeTree,
    phase_name: &str,
    phase: &nmbrs_workload::model::WorkloadPhase,
    phase_start_epoch_ms: u64,
    phase_component: &Arc<RwLock<nmbrs_metrics::component::Component>>,
) -> Result<(), String> {
    use polydat::ast::Value;
    // Numeric coercion for the pulled metric value — same policy as
    // `wrappers::metrics::value_to_f64` (Str / vector / none can't
    // become a metric value).
    fn to_f64(v: &Value) -> Option<f64> {
        match v {
            Value::F64(f) => Some(*f),
            Value::U64(u) => Some(*u as f64),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    // The phase scope kernel carries the synthesised
    // `__metric_<name>` outputs + the `phase_start` input. Its
    // presence is a synthesis invariant for a phase with metrics
    // (PolydatMatter::Definitions installs the kernel).
    let phase_kernel = scope_tree
        .phase_node_by_name(phase_name)
        .and_then(|idx| scope_tree.nodes[idx].cached_kernel.get().cloned())
        .ok_or_else(|| {
            format!(
                "phase '{phase_name}' has `metrics:` but no cached phase scope \
             kernel was installed — synthesis bug (a phase with metrics \
             classifies as PolydatMatter::Definitions and must install a \
             kernel via the `Bindings` install spec)"
            )
        })?;

    // Fresh subscope for the completion-time pull (own state, shares
    // the parent's cells). Mirrors the per-fiber main-kernel and the
    // do-loop persistent-kernel construction.
    let mut k = phase_kernel
        .build_subscope(
            polydat::kernel::subcontext::PolydatMatter::builder()
                .program(phase_kernel.program().clone())
                .build()
                .expect("program-form matter is infallible"),
        )
        .map_err(|e| format!("phase '{phase_name}': metric-pull subscope: {e}"))?;
    phase_kernel.propagate_inputs_into(&mut k);

    // The synthesized `phase_start` is no longer an extern filled here — it
    // binds `phase_start_millis()`, which reads the phase origin scoped by
    // `run_phase` and is therefore already correct at this pull AND for every
    // op that ran before it (as an extern it was set only here, so ops read the
    // 0 default). This remains only for a workload that declares the extern by
    // hand; a program without the input is the normal case and no-ops.
    if let Some(idx) = k.program().find_input("phase_start") {
        k.state().set_input(idx, Value::U64(phase_start_epoch_ms));
    }

    // Pull every metric value first (no component lock held), then
    // register + record under a single write lock. Stable name order
    // keeps emission deterministic.
    let mut entries: Vec<_> = phase.metrics.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut recorded: Vec<(
        String,
        nmbrs_workload::model::MetricSpec,
        f64,
        Option<nmbrs_metrics::labels::Labels>,
    )> = Vec::new();
    'metrics: for (name, spec) in entries {
        let binding = crate::scope::synthesize_metric_binding_name(name);
        let value = k.pull(&binding).clone();
        let Some(raw) = to_f64(&value) else {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "phase '{phase_name}' metric '{name}': value `{expr}` resolved to \
                 a non-numeric {disc:?}; skipping (metric values must be U64 / F64 / Bool)",
                expr = spec.value,
                disc = std::mem::discriminant(&value)
            );
            continue;
        };
        let sanitised = match &spec.format {
            Some(f) => match nmbrs_workload::metric_format::parse_format_spec(f) {
                Ok(fs) => fs.apply(raw),
                Err(e) => {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "phase '{phase_name}' metric '{name}' format '{f}': {e}; \
                         recording unformatted value"
                    );
                    raw
                }
            },
            None => raw,
        };
        // C9 — dimensional placement. Pull each synthesised
        // `__cell_<name>__<dim>` coordinate from the same subscope
        // (the phase synthesiser emits them alongside the value
        // binding), mirroring the op-level wrapper's semantics: a
        // dimension's value is a label value and therefore a STRING —
        // anything else is refused rather than stringified behind the
        // author's back.
        let coord = if spec.cell.is_empty() {
            None
        } else {
            let mut c = nmbrs_metrics::labels::Labels::default();
            for dim in spec.cell.keys() {
                let wire = crate::scope::synthesize_cell_binding_name(name, dim);
                let v = k.pull(&wire).clone();
                let Value::Str(text) = &v else {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "phase '{phase_name}' metric '{name}' cell '{dim}': \
                         coordinate resolved to a non-string {disc:?}; skipping \
                         the metric (dimension values are label values — convert \
                         the expression explicitly)",
                        disc = std::mem::discriminant(&v)
                    );
                    continue 'metrics;
                };
                c = c.with(dim.clone(), text.to_string());
            }
            Some(c)
        };
        recorded.push((name.clone(), spec.clone(), sanitised, coord));
    }

    // Register + record. An un-celled metric lands on the phase
    // component; a cell-placed metric lands on the coordinate's cell
    // component under it (C9 — one placement rule across the op and
    // phase tiers). Locks are taken per registration, never across
    // cell resolution.
    for (name, spec, value, coord) in recorded {
        use nmbrs_metrics::component::InstrumentRef;
        use nmbrs_workload::model::MetricKind;
        let family = spec.family.clone().unwrap_or_else(|| name.clone());
        let kind = spec.kind.unwrap_or_default();
        let target = match &coord {
            None => phase_component.clone(),
            Some(c) => nmbrs_metrics::cells::resolve_under(phase_component, c),
        };
        let target_labels = {
            let g = target.read().unwrap_or_else(|e| e.into_inner());
            g.effective_labels().clone()
        };
        let instr_labels = target_labels.with("family", family.clone());
        let instrument: InstrumentRef = match kind {
            MetricKind::Gauge => {
                let g = std::sync::Arc::new(nmbrs_metrics::instruments::gauge::ValueGauge::new(
                    instr_labels,
                ));
                g.set(value);
                InstrumentRef::Gauge(g)
            }
            MetricKind::Histogram => {
                let h = std::sync::Arc::new(nmbrs_metrics::instruments::histogram::Histogram::new(
                    instr_labels,
                ));
                h.record(value as u64);
                InstrumentRef::Histogram(h)
            }
            MetricKind::Counter => {
                let c = std::sync::Arc::new(nmbrs_metrics::instruments::counter::Counter::new(
                    instr_labels,
                ));
                if value > 0.0 {
                    c.inc_by(value as u64);
                }
                InstrumentRef::Counter(c)
            }
        };
        let mut g = target.write().unwrap_or_else(|e| e.into_inner());
        if let Err(e) =
            g.register_instrument_with_unit(family.clone(), spec.unit.clone(), instrument)
        {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "phase '{phase_name}' metric '{name}': instrument registration \
                 for family '{family}': {e}"
            );
        }
    }
    Ok(())
}

/// SRD-35 Push B helper — return the conventional
/// driver-selector parameter name for an adapter so the
/// pool's shared-attach lookup can resolve which
/// `DriverImpl` is in play. The convention is
/// `<adapter>driver=…` (e.g. `cqldriver=scylla`,
/// `httpdriver=…`); adapters that don't follow it never
/// have multiple driver impls today, so the synthesized
/// name resolves to `None` from `find_driver` and the
/// executor falls through to the legacy path harmlessly.
fn resolve_selector_param(adapter: &str) -> String {
    format!("{adapter}driver")
}

/// Build a runtime [`PhaseIdentity`] for a phase that has just
/// transitioned. The `yaml_path` is read from the global scene
/// tree; `coords` is the canonical phase-labels string the
/// runtime already produced via
/// [`format_scope_coordinate_path`]. `phase_hash` is `None`
/// here — the writer was already updated with the hash at
/// compile time via [`crate::checkpoint::CheckpointWriter::update_phase_hash`],
/// and identity equality is on `(yaml_path, coords)` regardless.
fn phase_identity_for(phase_name: &str, phase_labels: &str) -> crate::checkpoint::PhaseIdentity {
    let yaml_path = crate::scene_tree::current()
        .and_then(|t| {
            // Match against any status — the lookup may fire
            // before set_phase_running (declare path) or after
            // set_phase_completed (post-flush path).
            t.find_phase(phase_name, phase_labels, None)
                .and_then(|id| t.nodes.get(id).map(|n| n.yaml_path.clone()))
        })
        .unwrap_or_default();
    crate::checkpoint::PhaseIdentity {
        yaml_path,
        coords: phase_labels.to_string(),
        phase_hash: None,
    }
}

// Format bindings as a sorted labels string for stable matching.
//
// `format_scope_coordinate_path` lives on the Polydat side — see
// `polydat::kernel::format_scope_coordinate_path`. Re-exporting
// the path here would just be alias chrome; consumers in this crate
// import it directly from `polydat::kernel`.

/// SRD 71: decode a cursor's `over` clause result into a
/// concrete `(start_ord, end_ord)` narrowing range against the
/// cursor's declared extent.
///
/// Accepts:
/// - `Value::Str(spec)` — parse `spec` as a partition spec,
///   resolve against `[0, extent)`. Must be single-partition.
/// - `Value::Ext(Partition)` — already resolved, use directly.
/// - `Value::Ext(PartitionSpec)` — resolve against `[0, extent)`.
///   Must be single-partition.
/// - `Value::Ext(PartitionList)` — already resolved. Must hold
///   exactly one partition.
/// - `Value::None` — no narrowing applied.
///
/// A cursor consumes its `over` source directly, so a
/// multi-partition spec here is a startup error per SRD 71
/// §"Single-partition / no-iteration form" — only an enclosing
/// `for:` iteration can walk a multi-partition list, and the
/// diagnostic points the author at that form. Silently using
/// partition 0 would run a fraction of the requested work.
///
/// Returns `Ok(None)` for the no-narrowing case so the caller
/// keeps the cursor's full extent. Returns `Err` on a malformed
/// spec, an empty or multi-partition list, or an unsupported
/// value type.
///
/// `open_extent` marks `until_*` cursors: they have no closed
/// extent to resolve a spec against (their "extent" is just the
/// per-pass base chunk), so spec-shaped sources (Str /
/// PartitionSpec) are rejected with guidance, and
/// already-resolved Partition values pass through with their
/// absolute ordinals intact — no proportional reprojection, per
/// SRD 71's open-question resolution (recourse (b): resolve the
/// spec against an explicit reference extent first, e.g.
/// `partitions(spec, N)`).
fn resolve_over(
    value: &polydat::ast::Value,
    extent: u64,
    open_extent: bool,
) -> Result<Option<polydat::iteration::cursor_partition::Partition>, String> {
    use polydat::ast::Value;
    use polydat::iteration::cursor_partition::{Partition, parse, resolve};

    let single_of = |parts: Vec<Partition>| -> Result<Partition, String> {
        match parts.len() {
            0 => Err("partition list is empty — spec produced no partitions".to_string()),
            1 => Ok(parts.into_iter().next().unwrap()),
            n => Err(format!(
                "spec resolves to {n} partitions, but this cursor consumes it \
                 directly (no enclosing `for:` iteration). Iterate the list with \
                 `for: \"p in <param>.partitions\"` and declare the cursor \
                 `over p`, or supply a single-partition spec"
            )),
        }
    };

    // SRD 71: when a `Partition` value flows into `over` from a
    // comprehension iter-var or sibling cursor's `.cursor`
    // projection, its `base_extent` may not match the consuming
    // cursor's extent (e.g. `partitions("linear:3", 100)` then
    // used by a cursor of extent 1000). Re-resolve from the
    // partition's percentage bounds against the cursor's actual
    // extent — that's the cursor's "narrow by this partition"
    // contract. When `base_extent == extent`, the ordinals are
    // already correct; pass through unchanged.
    let reproject = |p: &Partition| -> Partition {
        // Open-extent cursors have no meaningful resolution
        // target — the resolved partition's absolute ordinals
        // ARE the bounds (SRD 71: the partition's cardinality
        // becomes the hard upper bound the policy converges
        // toward).
        if open_extent || p.base_extent == extent || extent == 0 {
            return *p;
        }
        let start_ord = ((p.start_pct / 100.0) * extent as f64).round() as u64;
        let end_ord = ((p.end_pct / 100.0) * extent as f64).round() as u64;
        Partition {
            idx: p.idx,
            count: p.count,
            start_ord,
            end_ord,
            start_pct: p.start_pct,
            end_pct: p.end_pct,
            base_extent: extent,
        }
    };

    let reject_spec_for_open = || -> String {
        "an open-extent cursor (`until_*`) has no extent to resolve a \
         partition spec against — its declared size is just the per-pass \
         base chunk. Resolve the spec against an explicit reference extent \
         first (`for: \"p in partitions(<spec>, <extent>)\"`) and declare \
         the cursor `over p`"
            .to_string()
    };

    match value {
        Value::None => Ok(None),
        Value::Str(s) => {
            if open_extent {
                return Err(reject_spec_for_open());
            }
            let spec = parse(s.as_ref())?;
            let parts = resolve(&spec, 0, extent)?;
            Ok(Some(single_of(parts)?))
        }
        Value::Ext(b) => {
            if let Some(p) = value.as_partition() {
                Ok(Some(reproject(p)))
            } else if let Some(spec) = value.as_partition_spec() {
                if open_extent {
                    return Err(reject_spec_for_open());
                }
                let parts = resolve(spec, 0, extent)?;
                Ok(Some(single_of(parts)?))
            } else if let Some(list) = value.as_partition_list() {
                let single = single_of(list.as_slice().to_vec())?;
                Ok(Some(reproject(&single)))
            } else {
                Err(format!(
                    "`over` expression produced an Ext value of unexpected type `{}` — \
                     expected Partition, PartitionSpec, or PartitionList",
                    b.type_name(),
                ))
            }
        }
        other => Err(format!(
            "`over` expression produced unsupported value type — expected Str or partition-typed Ext, got {other:?}"
        )),
    }
}

/// Extract a human-readable message from a `catch_unwind` payload.
/// Used by the init-binding scope-activation check (SRD 11
/// §"Init Binding Contract" Plan B) to surface eval panics as
/// clean error messages rather than re-panicking the executor.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
