// Copyright (c) nosqlbench
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
// implied. See the License for the specific language governing
// permissions and limitations under the License.

//! SRD-83 — stop conditions. **Steps 1–2: runtime-state wires + the
//! scoped predicate.**
//!
//! A stop-condition predicate (SRD-83 Part 2) reads a shell's live
//! state — how many ops ran, how many errored, how long it's been
//! going. Those values are exposed as **volatile externs**; the
//! executor injects a [`RuntimeState`] snapshot before each evaluation.
//!
//! The predicate is compiled (`compile_stop_condition`) as an SRD-84
//! **shape 2** [`ScopedExpr`] bound to the shell's phase kernel — a
//! scope-bound, callable expression over the runtime-state externs —
//! *not* baked into the phase kernel's own matter. The firing events /
//! settle daemon (step 3) drive [`RuntimeState::trips`] per trigger.

use std::sync::Arc;

use polydat::ast::Value;
use polydat::dsl::stub::{ExprStub, GraphMatter, ScopedExpr};
use polydat::kernel::{Dataflow, PolydatKernel};

use crate::phase_outcome::Outcome;

/// Canonical runtime-state wire names a stop-condition predicate may
/// read. A predicate references the ones it needs; the rest are absent
/// from its kernel and skipped at injection.
pub mod wire {
    //! Predicate vocabulary. Every counter wire is named EXACTLY as
    //! its registered instrument (`ActivityMetrics::register_on`), so
    //! the guard language and the metric namespace are one vocabulary
    //! — what you see in metrics.db / the status line is what you can
    //! guard on. No derived pseudo-counters and no precomputed rates:
    //! a rate is written in the predicate from these base counters
    //! (e.g. `to_f64(result_failure) > to_f64(cycles_total) * 0.05`).
    //! Anything beyond this per-shell fast path (timers, windows,
    //! other phases) reads through the `metric(...)` /
    //! `metric_window(...)` reader nodes — predicates are full
    //! polydat.

    /// Cycles completed so far — the `cycles_total` instrument.
    pub const CYCLES_TOTAL: &str = "cycles_total";
    /// Terminal failed ops so far — the `result_failure` instrument.
    /// An op that exhausts its whole `tries` budget counts once here;
    /// transient failures that a retry absorbs never do.
    pub const RESULT_FAILURE: &str = "result_failure";
    /// Resolved attempts — the `attempt_total` instrument. Counted at
    /// RESOLUTION (same discipline as the result instruments), so
    /// `attempt_total == attempt_success + attempt_failure` at every
    /// read and it is the exact denominator for attempt rates.
    pub const ATTEMPT_TOTAL: &str = "attempt_total";
    /// Resolved successful attempts — the `attempt_success` instrument.
    pub const ATTEMPT_SUCCESS: &str = "attempt_success";
    /// Resolved failed attempts (terminal or not) — the
    /// `attempt_failure` instrument. `attempt_success +
    /// attempt_failure` is the resolved-attempt denominator;
    /// in-flight attempts are deliberately not represented (a rate
    /// over dispatch-time counts skews low by the in-flight window).
    pub const ATTEMPT_FAILURE: &str = "attempt_failure";
    /// Wall time since the shell started (`u64`, milliseconds).
    /// Shell state, not an instrument.
    pub const ELAPSED_MS: &str = "elapsed_ms";
    /// Scenario shells — child phases declared / failed / finished.
    /// Shell state, not instruments.
    pub const CHILDREN_TOTAL: &str = "children_total";
    pub const CHILDREN_FAILED: &str = "children_failed";
    pub const CHILDREN_DONE: &str = "children_done";
}

/// A snapshot of a shell's runtime state, injected into its
/// stop-condition predicate kernel before each evaluation. SRD-83
/// Part 2. Cheap to copy; the executor builds one from the live
/// activity counters (`cycles_completed` / `errors_total`) plus the
/// phase clock, and from child outcomes for a scenario shell.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RuntimeState {
    pub cycles_total: u64,
    pub result_failure: u64,
    pub elapsed_ms: u64,
    pub attempt_total: u64,
    pub attempt_success: u64,
    pub attempt_failure: u64,
    pub children_total: u64,
    pub children_failed: u64,
    pub children_done: u64,
}

impl RuntimeState {
    /// Terminal-failure fraction for the human trip message ONLY —
    /// derived here exactly as a predicate would derive it
    /// (`result_failure / cycles_total`). Predicates never read a
    /// precomputed rate: they write the division themselves from the
    /// instrument-named wires.
    fn result_failure_fraction(&self) -> f64 {
        if self.cycles_total == 0 {
            0.0
        } else {
            self.result_failure as f64 / self.cycles_total as f64
        }
    }

    /// Attempt-failure fraction for the trip message — failed over
    /// resolved attempts (`attempt_failure / (attempt_success +
    /// attempt_failure)`), the see-through-retries view.
    fn attempt_failure_fraction(&self) -> f64 {
        if self.attempt_total == 0 {
            0.0
        } else {
            self.attempt_failure as f64 / self.attempt_total as f64
        }
    }

    /// Human-readable snapshot of the live wires at evaluation time, for a
    /// tripped condition's message — so the failure reports the ACTUAL
    /// values (`op_count=523, errors=22, error_rate=4.21%`) that crossed the
    /// threshold, not just the predicate text. Adapts to the shell: a
    /// scenario/workload shell reports `children_*`; a phase shell reports
    /// the op / error counts.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.children_total > 0 {
            parts.push(format!(
                "children_done={}/{}",
                self.children_done, self.children_total
            ));
            if self.children_failed > 0 {
                parts.push(format!("children_failed={}", self.children_failed));
            }
        } else {
            parts.push(format!("cycles_total={}", self.cycles_total));
            parts.push(format!("result_failure={}", self.result_failure));
            parts.push(format!(
                "result_failure/cycles_total={:.2}%",
                self.result_failure_fraction() * 100.0
            ));
            // Attempts earn a mention when they carry signal: retries
            // active (resolved attempts exceed cycles) or any attempt
            // failed (an attempt-wire guard may have tripped).
            if self.attempt_total > self.cycles_total || self.attempt_failure > 0 {
                parts.push(format!(
                    "attempt_failure={}/{}",
                    self.attempt_failure, self.attempt_total
                ));
                parts.push(format!(
                    "attempt_failure_fraction={:.2}%",
                    self.attempt_failure_fraction() * 100.0
                ));
            }
        }
        parts.push(format!("elapsed={:.1}s", self.elapsed_ms as f64 / 1000.0));
        parts.join(", ")
    }

    /// The `(wire, value)` pairs this snapshot supplies. The single
    /// source for the wire→`Value` mapping, so the injector and any
    /// extern-declaration list stay in agreement.
    fn wires(&self) -> [(&'static str, Value); 9] {
        [
            (wire::CYCLES_TOTAL, Value::U64(self.cycles_total)),
            (wire::RESULT_FAILURE, Value::U64(self.result_failure)),
            (wire::ELAPSED_MS, Value::U64(self.elapsed_ms)),
            (wire::ATTEMPT_TOTAL, Value::U64(self.attempt_total)),
            (wire::ATTEMPT_SUCCESS, Value::U64(self.attempt_success)),
            (wire::ATTEMPT_FAILURE, Value::U64(self.attempt_failure)),
            (wire::CHILDREN_TOTAL, Value::U64(self.children_total)),
            (wire::CHILDREN_FAILED, Value::U64(self.children_failed)),
            (wire::CHILDREN_DONE, Value::U64(self.children_done)),
        ]
    }

    /// Inject this snapshot into a predicate kernel: for each runtime-
    /// state wire the program declares as an input, write the current
    /// value. Wires the predicate doesn't reference are absent
    /// (`find_input` → `None`) and skipped; a type-mismatch on a
    /// mis-declared extern is swallowed (the step-2 predicate compile
    /// declares the externs with matching types).
    pub fn inject_into<D: Dataflow>(&self, ctx: &mut D) {
        for (name, value) in self.wires() {
            if let Some(idx) = ctx.find_input(name) {
                let _ = ctx.set_wire_idx(idx, value);
            }
        }
    }

    /// Evaluate a stop-condition predicate against this snapshot:
    /// inject the runtime-state wires, then read the predicate's
    /// truthiness. `true` means the condition has tripped.
    pub fn trips(&self, condition: &mut ScopedExpr) -> bool {
        self.inject_into(condition.dataflow());
        condition.is_true()
    }
}

/// Graph matter (SRD-84 shape 1) declaring the runtime-state wires as
/// typed externs, in a stable order with types matching
/// [`RuntimeState::wires`]. Built — not string-parsed — so a
/// stop-condition predicate compiled over it can reference any wire;
/// `inject_into` / `trips` then populate the ones it actually uses.
pub fn extern_matter() -> GraphMatter {
    let mut m = GraphMatter::new();
    m.extern_wire::<u64>(wire::CYCLES_TOTAL)
        .extern_wire::<u64>(wire::RESULT_FAILURE)
        .extern_wire::<u64>(wire::ELAPSED_MS)
        .extern_wire::<u64>(wire::ATTEMPT_TOTAL)
        .extern_wire::<u64>(wire::ATTEMPT_SUCCESS)
        .extern_wire::<u64>(wire::ATTEMPT_FAILURE)
        .extern_wire::<u64>(wire::CHILDREN_TOTAL)
        .extern_wire::<u64>(wire::CHILDREN_FAILED)
        .extern_wire::<u64>(wire::CHILDREN_DONE);
    m
}

/// Compile a stop-condition predicate (SRD-83 Part 2) as an SRD-84
/// **shape 2** [`ScopedExpr`] bound to the shell's `phase_kernel`: a
/// `volatile` expression living in that kernel's lexical scope, over
/// the runtime-state externs, coerced to `u64` truthiness. The executor
/// (step 3) evaluates it per trigger via [`RuntimeState::trips`].
///
/// The predicate is *not* baked into the phase kernel's own matter — it
/// is a separate sub-context, so authored phase bindings and evaluated
/// stop predicates stay orthogonal concerns.
pub fn compile_stop_condition(
    phase_kernel: &PolydatKernel,
    idx: usize,
    when: &str,
) -> Result<ScopedExpr, String> {
    let name = format!("__stop_cond_{idx}");
    let mut matter = extern_matter();
    // A predicate may also read `shared` wires from the scope cascade —
    // the windowed-backstop shape: a daemon computes trailing-window
    // state into root shared cells and the loader's stop condition
    // guards on them. Those names are NOT in the phase kernel's own
    // matter (nothing there references them), so the subscope compile
    // cannot resolve them from the parent snapshot — but the cascade's
    // cell carrier IS transitive (`shared_cells_in_scope` includes
    // ancestors' cells even when this scope never names them), and an
    // extern declared here attaches to the in-scope cell by name at
    // subscope build. Declare one typed extern per referenced in-scope
    // cell, AT THE CELL'S OWN PORT TYPE (an f64 cell read through a
    // u64 extern would corrupt the comparison). The predicate then
    // reads the LIVE cell on every evaluation — `volatile` below keeps
    // it out of const-fold — so a daemon's write between ticks is seen
    // without rebinding. Names that are neither canonical runtime
    // wires nor in-scope cells stay undeclared: a misspelling must
    // remain a loud compile error, not a silently-0 extern.
    // (Regression 2026-08-06: every adaptive run since the windowed
    // backstop landed logged "unknown wire: 'recent_result_failures'"
    // per tier and ran with NO stop conditions armed.)
    const CANONICAL: [&str; 9] = [
        wire::CYCLES_TOTAL,
        wire::RESULT_FAILURE,
        wire::ELAPSED_MS,
        wire::ATTEMPT_TOTAL,
        wire::ATTEMPT_SUCCESS,
        wire::ATTEMPT_FAILURE,
        wire::CHILDREN_TOTAL,
        wire::CHILDREN_FAILED,
        wire::CHILDREN_DONE,
    ];
    let cells = phase_kernel.shared_cells_in_scope();
    for referenced in polydat::dsl::refs::referenced_names(when) {
        if CANONICAL.contains(&referenced.as_str()) {
            continue;
        }
        if let Some(cell) = cells.iter().find(|c| c.name == referenced) {
            matter.extern_wire_typed(&cell.name, cell.port_type);
        }
    }
    matter.bind(
        ExprStub::parse(&name, when)
            .map_err(|e| format!("stop condition {idx} predicate `{when}`: {e}"))?
            .returning::<u64>()
            .volatile(),
    );
    ScopedExpr::bind(phase_kernel, name, matter)
        .map_err(|e| format!("stop condition {idx} predicate `{when}`: {e}"))
}

/// SRD-101 — compile a `continue_if` predicate into a gate kernel with a single
/// output, `__continue_if := <when>`.
///
/// The iteration coordinates (`p`, `k`, …) are pre-declared as `extern`s **at
/// their runtime types** (from `coords`' [`Value::port_type`]) so the compiler
/// does not default them — an `Ext<Partition>` coordinate must be typed `ext`,
/// not `u64`, for `end_of(p)` to type-check. Every OTHER free identifier the
/// predicate reads — outer-scope consts like `effective_max_size` — is left to
/// the compiler's auto-extern; [`eval_continue_if`] then wires those in from the
/// parent scope's cascade at materialisation. `coords` is a representative
/// iteration's bindings (all iterations of a comprehension share coordinate
/// names + types).
pub fn compile_continue_if(
    when: &str,
    coords: &[(String, Value)],
    strict: bool,
) -> Result<Arc<PolydatKernel>, String> {
    let mut source = String::new();
    for (name, value) in coords {
        source.push_str(&format!(
            "extern {name}: {}\n",
            value.port_type().to_keyword()
        ));
    }
    source.push_str(&format!("__continue_if := {when}"));
    let options = polydat::dsl::compile::CompileOptions {
        required_outputs: vec!["__continue_if".to_string()],
        strict,
        ..Default::default()
    };
    polydat::dsl::compile::compile_polydat_with_options(&source, &options, None)
        .map(Arc::new)
        .map_err(|e| format!("continue_if predicate `{when}`: {e}"))
}

/// SRD-101 — evaluate a compiled `continue_if` gate for ONE iteration.
///
/// Materialises the gate kernel as a proper sub-scope of the sweep's `parent`
/// — the SAME scope-walk that builds the body's per-iteration kernel
/// ([`PolydatKernel::for_iteration`]) — so the auto-externed outer consts are
/// WIRED IN from the parent's cascade and the iteration coordinates are SET
/// from `bindings`. The predicate therefore resolves every in-scope name
/// natively: the canonical-scope contract (one scope-walked kernel answers all
/// names) rather than a hand-declared-extern sub-context that can't see
/// inherited consts. Returns truthiness: `true` → keep sweeping (run this
/// iteration); `false` → the gate has gone false, halt the sweep.
pub fn eval_continue_if(
    gate_canonical: &Arc<PolydatKernel>,
    parent: &Arc<PolydatKernel>,
    bindings: &[(String, Value)],
) -> Result<bool, String> {
    let mut kernel = PolydatKernel::for_iteration(gate_canonical, parent, bindings);
    let pulled = Arc::get_mut(&mut kernel)
        .ok_or("continue_if: freshly built gate kernel unexpectedly shared")?
        .pull("__continue_if");
    Ok(match pulled {
        Value::Bool(b) => *b,
        other => other.as_u64() != 0,
    })
}

/// A declared stop condition resolved to its predicate text and its
/// SRD-83 Part 5 `effect` — the two-axis [`Outcome`] the shell adopts
/// when the predicate trips. Built from a `StopConditionSpec` at the
/// gather sites (executor / runner), so this module need not depend on
/// the workload model's spec shape.
/// SRD-83 follow-up — WHERE a tripped stop condition's effect lands
/// (its action scope), decoupled from WHERE it was detected. Resolved at
/// the gather sites from the spec's `at:` (default = the innermost, most
/// specific level of `per:`), and consumed at the trip site to pick the
/// cooperative stop handle. This is the runtime "sink level"; the
/// gather sites map the workload-model `ScopeLevel` onto it, so this
/// module stays free of the model's spec shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopScope {
    /// Act on the declaring / phase scope (the historical default): halt
    /// just this phase's drain loop (the activity `stop_flag`).
    #[default]
    Phase,
    /// Halt the enclosing scenario sweep. (In C this reuses the workload
    /// `walk_stop` latch — a scenario-only handle is a later refinement.)
    Scenario,
    /// Halt the enclosing workload shell (its `walk_stop`), leaving the
    /// process/session running.
    Workload,
}

#[derive(Debug, Clone)]
pub struct StopConditionDecl {
    /// The polydat predicate over runtime-state wires.
    pub when: String,
    /// The Outcome the shell adopts when `when` trips.
    pub effect: Outcome,
    /// Reason class recorded when the condition trips. `None` →
    /// the default `stop_condition: {when}`. Synthesized guards
    /// set an explicit class (e.g. `error_rate_exceeded`) so trip
    /// reporting keeps its established vocabulary.
    pub reason: Option<String>,
    /// SRD-83 follow-up — the action scope the effect lands on. Detection
    /// stays at the gather scope; this is where the *action* is routed.
    /// Defaults to `Phase` (act in place = historical behaviour).
    pub target: StopScope,
    /// SRD-83 follow-up — `action: abort`. When set, the halt escalates to
    /// CANCELLING in-flight ops (arms the session cancel-ops countdown at
    /// the trip site), not just cooperatively draining them. `false` for
    /// the cooperative `stop`/`fail` actions.
    pub cancel_ops: bool,
}

impl StopConditionDecl {
    /// SRD-82 §"AggregateGuard retired as a default" — the visible
    /// synthesized guard an opted-in `error_rate_max` becomes. ONE
    /// canonical construction: the expression string exists here
    /// and nowhere else, and the caller logs it at synthesis so
    /// the operator sees the condition before it can trip.
    ///
    /// `error_rate` is a PROPORTION in [0,1] (terminal errors over
    /// ops), so a bound of 1.0 simply never fires — a Control-class
    /// phase that deliberately dwells at a saturating setting uses
    /// `error_rate_max: 1.0` to opt out with no special-casing. The
    /// 50-op floor keeps a single early error from aborting a phase
    /// before the rate is meaningful.
    pub fn error_rate_guard(max: f64) -> Self {
        Self {
            when: format!(
                "cycles_total >= 50 && \
                 to_f64(result_failure) > (to_f64(cycles_total) * {max})"
            ),
            effect: Outcome::failed(),
            reason: Some("error_rate_exceeded".to_string()),
            target: StopScope::Phase,
            cancel_ops: false,
        }
    }

    /// SRD-83 governance `timeout:` (GAP-12) — the visible synthesized
    /// guard a phase `timeout:` becomes. ONE canonical construction,
    /// mirroring [`Self::error_rate_guard`]: the expression string
    /// exists here and nowhere else, and the caller logs it at
    /// synthesis. Expiry is the protocol OUT-OF-RANGE disposition —
    /// Interrupted+Failed with reason class `timeout` — distinguishing
    /// "disqualified at this tier" from every other failure class. A
    /// clean time-boxed BUDGET is not this: that's a bounded cursor or
    /// a `stop_when … effect: stop`.
    pub fn timeout_guard(timeout_ms: u64) -> Self {
        Self {
            when: format!("elapsed_ms > {timeout_ms}"),
            effect: Outcome::failed(),
            reason: Some("timeout".to_string()),
            target: StopScope::Phase,
            cancel_ops: false,
        }
    }

    /// Map an SRD-83 `action:`/`effect:` string to its [`Outcome`]. `"stop"`
    /// is a clean halt (`Interrupted + Succeeded`, keep the partial result);
    /// `"fail"` and `"abort"` are failure halts (`Interrupted + Failed`).
    /// `None` — and any unrecognized value — resolves to `default`, which the
    /// caller sets per shell level (phase trips default to `fail`, workload
    /// trips default to a graceful `stop`). `abort` differs from `fail` only
    /// in its aggression — same failing `Outcome`, but it also cancels
    /// in-flight ops (see [`action_cancels_ops`]); the split is carried
    /// separately so this stays a pure verb→outcome map. Action-string
    /// validation proper belongs to workload load (dryrun).
    pub fn effect_from_str(effect: Option<&str>, default: Outcome) -> Outcome {
        match effect {
            Some("stop") => Outcome::interrupted(),
            Some("fail") | Some("abort") => Outcome::failed(),
            _ => default,
        }
    }

    /// SRD-83 follow-up — does this `action:` verb escalate to cancelling
    /// in-flight ops? Only `abort` does; `stop`/`fail` (and the default)
    /// drain cooperatively. One canonical predicate so every gather site
    /// classifies the verb identically.
    pub fn action_cancels_ops(action: Option<&str>) -> bool {
        matches!(action, Some("abort"))
    }
}

/// A shell's compiled stop conditions (SRD-83 steps 3–5): the default
/// `error_rate > error_rate_max` condition (when a max is set) plus each
/// phase-declared predicate, every one a scope-bound [`ScopedExpr`].
/// Built once when the shell's kernel exists; evaluated per firing event
/// against a [`RuntimeState`] snapshot. This is the polydat-predicate
/// successor to the interim `AggregateGuard`.
pub struct StopConditionSet {
    conditions: Vec<StopCondition>,
}

struct StopCondition {
    expr: ScopedExpr,
    /// SRD-83 Part 5 — the two-axis Outcome the shell adopts when this
    /// condition trips. `fail` → Interrupted+Failed; `stop` →
    /// Interrupted+Succeeded.
    effect: Outcome,
    /// The error/reason class recorded when this condition trips.
    reason: String,
    /// SRD-83 follow-up — the action scope the effect is routed to.
    target: StopScope,
    /// SRD-83 follow-up — `action: abort` escalates to cancelling in-flight
    /// ops at the trip site (not just cooperative drain).
    cancel_ops: bool,
}

impl StopConditionSet {
    /// Build the set for a shell whose kernel is `phase_kernel`: one
    /// uniform loop over the declared conditions — synthesized guards
    /// (e.g. [`StopConditionDecl::error_rate_guard`]) arrive in the
    /// SAME list as author-declared predicates, inserted at config
    /// assembly where they are logged (SRD-82 §"AggregateGuard
    /// retired as a default": no hidden conditions). A predicate that
    /// fails to compile is a hard error (surfaced at build time, not
    /// swallowed per "never ignore silently").
    pub fn build_for_phase(
        phase_kernel: &PolydatKernel,
        declared: &[StopConditionDecl],
    ) -> Result<Self, String> {
        let mut conditions = Vec::new();
        for (idx, decl) in declared.iter().enumerate() {
            let expr = compile_stop_condition(phase_kernel, idx, &decl.when)?;
            conditions.push(StopCondition {
                expr,
                effect: decl.effect.clone(), // Outcome no longer Copy (SRD-92 reason field)
                reason: decl
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("stop_condition: {}", decl.when)),
                target: decl.target,
                cancel_ops: decl.cancel_ops,
            });
        }
        Ok(Self { conditions })
    }

    /// An installed-nothing set (used as the safe fallback when a
    /// predicate fails to compile at runtime).
    pub fn empty() -> Self {
        Self {
            conditions: Vec::new(),
        }
    }

    /// True when no condition is installed (skip evaluation entirely).
    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }

    /// Evaluate every condition against `state`; return the error class
    /// of the first that trips, or `None`. (First-match: any trip stops
    /// the shell, so the order only affects which reason is recorded.)
    pub fn evaluate(&mut self, state: &RuntimeState) -> Option<(Outcome, String, StopScope, bool)> {
        for cond in &mut self.conditions {
            if state.trips(&mut cond.expr) {
                return Some((
                    cond.effect.clone(),
                    cond.reason.clone(),
                    cond.target,
                    cond.cancel_ops,
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression 2026-08-06 — the windowed-backstop shape: a stop
    /// predicate reading root `shared` wires the phase matter never
    /// mentions (a daemon computes trailing-window state into the
    /// cells; the loader guards on them). The subscope compile used to
    /// fail with "unknown wire" — logged as an ERROR at activity start
    /// and swallowed, so every adaptive run since the backstop landed
    /// ran with NO stop conditions armed. compile_stop_condition now
    /// declares typed externs for referenced in-scope cells, which
    /// attach to the cells at subscope build (the cascade's cell
    /// carrier is transitive) and read them LIVE.
    #[test]
    fn stop_condition_reads_root_shared_wire() {
        // Root scope: f64 cells, as compaction_demo_derived declares
        // (`shared recent_result_failures: f64 := 0.0`) — the typed-
        // extern path matters, a u64 guess would corrupt the compare.
        let mut root = polydat::dsl::compile_polydat(
            "shared recent_result_failures: f64 := 0.0
             shared recent_result_total: f64 := 0.0
             rx := 1",
        )
        .expect("root kernel");
        // Phase kernel as a subscope whose matter does NOT reference
        // the shared wires (like load_increment_adaptive's bindings).
        let pm = polydat::kernel::subcontext::PolydatMatter::builder()
            .label("phase_test")
            .source("x := 5")
            .build()
            .expect("matter");
        let phase = root.build_subscope(pm).expect("phase kernel");

        // Canonical-only predicates keep compiling.
        compile_stop_condition(&phase, 0, "result_failure >= 100")
            .expect("canonical-only predicate must compile");

        // The field predicate (5bd8b53's windowed backstop, verbatim
        // shape): canonical wires AND root shared cells in one text.
        let mut cond = compile_stop_condition(
            &phase, 0,
            "((result_failure >= 100) & (to_f64(result_failure) > (to_f64(cycles_total) * 0.025))) \
             | ((recent_result_failures >= 100.0) & (recent_result_failures > (recent_result_total + 1.0) * 0.025))")
            .expect("shared-wire predicate must compile");

        // LIVE-READ proof: cells at 0 → no trip...
        let state = RuntimeState {
            cycles_total: 1000,
            result_failure: 5,
            ..Default::default()
        };
        assert!(!state.trips(&mut cond), "cells at 0 must not trip");
        // ...a daemon-style write through the ROOT cell trips it
        // WITHOUT rebinding. A detached extern (default 0) would stay
        // false forever — the silent-disarm failure this test pins.
        let idx = root
            .program()
            .find_input("recent_result_failures")
            .expect("root input slot for the shared wire");
        root.state().set_input(idx, polydat::ast::Value::F64(150.0));
        assert!(
            state.trips(&mut cond),
            "predicate must read the LIVE shared cell (150 >= 100, ratio floor 0)"
        );
        // And back down: no latch.
        root.state().set_input(idx, polydat::ast::Value::F64(0.0));
        assert!(!state.trips(&mut cond), "cell reset must un-trip");

        // A misspelled name is neither canonical nor a cell: still a
        // loud compile error, never a silently-0 extern.
        assert!(
            compile_stop_condition(&phase, 0, "recent_result_failurez >= 1").is_err(),
            "unknown names must stay compile errors"
        );
    }

    #[test]
    fn failure_fraction_is_safe_at_zero_cycles() {
        assert_eq!(RuntimeState::default().result_failure_fraction(), 0.0);
        let s = RuntimeState {
            cycles_total: 100,
            result_failure: 10,
            ..Default::default()
        };
        assert_eq!(s.result_failure_fraction(), 0.1);
        let s = RuntimeState {
            cycles_total: 50,
            result_failure: 50,
            ..Default::default()
        };
        assert_eq!(s.result_failure_fraction(), 1.0);
    }

    #[test]
    fn error_rate_is_faithful_and_in_range_for_terminal_failures() {
        // `error_count` is the count of ops that TERMINALLY failed (≤
        // `op_count`), so the faithful `error_count / op_count` is in [0,1]
        // by construction — even a fully-failing phase reads exactly 1.0,
        // never above. The default `error_rate_max: 1.0` guard
        // (`op_count >= 50 && error_rate > 1.0`) therefore never trips, as
        // documented ("allow 100% — never trip") — without clamping.
        let all_fail = RuntimeState {
            cycles_total: 100,
            result_failure: 100,
            ..Default::default()
        };
        assert_eq!(all_fail.result_failure_fraction(), 1.0);
        let phase_kernel =
            polydat::dsl::compile_polydat("input cycle: u64\nx := 5").expect("phase kernel");
        let mut cond = compile_stop_condition(
            &phase_kernel,
            0,
            "cycles_total >= 50 && to_f64(result_failure) > (to_f64(cycles_total) * 1.0)",
        )
        .expect("compile scoped stop condition");
        assert!(
            !all_fail.trips(&mut cond),
            "error_rate_max:1.0 must never trip"
        );
        // A 0.5 guard still trips at >50% terminal failures.
        let mut half = compile_stop_condition(
            &phase_kernel,
            0,
            "cycles_total >= 50 && to_f64(result_failure) > (to_f64(cycles_total) * 0.5)",
        )
        .unwrap();
        assert!(
            RuntimeState {
                cycles_total: 100,
                result_failure: 60,
                ..Default::default()
            }
            .trips(&mut half)
        );
    }

    #[test]
    fn injects_referenced_wires_by_name_and_re_evaluates() {
        // A predicate that reads two runtime-state wires; the injector
        // must reach exactly those, by name. `volatile` forces a fresh
        // evaluation on each pull (as a stop-condition predicate does
        // per trigger).
        let src = "\
            extern cycles_total: u64 = 0\n\
            extern result_failure: u64 = 0\n\
            volatile sum := cycles_total + result_failure";
        let mut k = polydat::dsl::compile_polydat(src).expect("compile predicate kernel");

        RuntimeState {
            cycles_total: 10,
            result_failure: 5,
            ..Default::default()
        }
        .inject_into(&mut k);
        assert_eq!(*k.pull("sum"), Value::U64(15));

        RuntimeState {
            cycles_total: 40,
            result_failure: 2,
            ..Default::default()
        }
        .inject_into(&mut k);
        assert_eq!(*k.pull("sum"), Value::U64(42));
    }

    #[test]
    fn compiles_and_trips_a_scoped_stop_condition() {
        // SRD-83 Part 2 re-pointed onto SRD-84 shape 2: the predicate is
        // a `ScopedExpr` bound to the phase kernel, evaluated per trigger
        // against an injected runtime-state snapshot — never baked into
        // the phase matter.
        let phase_kernel =
            polydat::dsl::compile_polydat("input cycle: u64\nx := 5").expect("phase kernel");
        let mut cond = compile_stop_condition(
            &phase_kernel,
            0,
            "cycles_total > 50 && to_f64(result_failure) > (to_f64(cycles_total) * 0.1)",
        )
        .expect("compile scoped stop condition");
        // op_count under threshold → does not trip (error_rate is 0.5
        // here, but the `&&` short of op_count > 50 fails).
        assert!(
            !RuntimeState {
                cycles_total: 40,
                result_failure: 20,
                ..Default::default()
            }
            .trips(&mut cond)
        );
        // op_count 100 (> 50) and error_rate 0.2 (> 0.1) → trips.
        assert!(
            RuntimeState {
                cycles_total: 100,
                result_failure: 20,
                ..Default::default()
            }
            .trips(&mut cond)
        );
        // op_count over but error_rate 0.01 under → does not trip.
        assert!(
            !RuntimeState {
                cycles_total: 100,
                result_failure: 1,
                ..Default::default()
            }
            .trips(&mut cond)
        );
    }

    #[test]
    fn stop_condition_set_installs_default_error_rate_and_declared_predicates() {
        let phase_kernel =
            polydat::dsl::compile_polydat("input cycle: u64\nx := 5").expect("phase kernel");
        // The synthesized error-rate guard (0.1) rides the SAME list
        // as the declared op-count predicate — one uniform path
        // (SRD-82: no hidden conditions).
        let mut set = StopConditionSet::build_for_phase(
            &phase_kernel,
            &[
                StopConditionDecl::error_rate_guard(0.1),
                StopConditionDecl {
                    when: "cycles_total > 1000".to_string(),
                    effect: Outcome::failed(),
                    reason: None,
                    target: StopScope::Phase,
                    cancel_ops: false,
                },
            ],
        )
        .expect("build set");
        // The compiled set is independent of the parent kernel — drop it
        // and evaluation still works (each predicate is its own
        // sub-context). This is what lets the activity bind against a
        // throwaway root until the phase kernel is plumbed.
        drop(phase_kernel);
        assert!(!set.is_empty());
        // Below the 50-op floor: even 100% errors does not trip yet.
        assert!(
            set.evaluate(&RuntimeState {
                cycles_total: 10,
                result_failure: 10,
                ..Default::default()
            })
            .is_none()
        );
        // 5% errors, 100 ops → neither trips.
        assert!(
            set.evaluate(&RuntimeState {
                cycles_total: 100,
                result_failure: 5,
                ..Default::default()
            })
            .is_none()
        );
        // 20% errors → the default error-rate condition trips.
        assert_eq!(
            set.evaluate(&RuntimeState {
                cycles_total: 100,
                result_failure: 20,
                ..Default::default()
            }),
            Some((
                Outcome::failed(),
                "error_rate_exceeded".to_string(),
                StopScope::Phase,
                false
            ))
        );
        // Low errors but op_count over 1000 → the declared predicate trips.
        assert_eq!(
            set.evaluate(&RuntimeState {
                cycles_total: 2000,
                result_failure: 1,
                ..Default::default()
            }),
            Some((
                Outcome::failed(),
                "stop_condition: cycles_total > 1000".to_string(),
                StopScope::Phase,
                false
            ))
        );
    }

    /// SRD-83 follow-up — `action: abort` classifies as a FAILED outcome
    /// (same validity as `fail`) but carries the `cancel_ops` escalation
    /// bit; `stop`/`fail`/default do not. The verb→outcome map and the
    /// cancel-ops classifier are the two halves the gather sites compose.
    #[test]
    fn abort_action_is_failed_and_cancels_ops() {
        // Verb → outcome: abort shares fail's failing outcome.
        assert_eq!(
            StopConditionDecl::effect_from_str(Some("abort"), Outcome::interrupted()),
            Outcome::failed()
        );
        assert_eq!(
            StopConditionDecl::effect_from_str(Some("fail"), Outcome::interrupted()),
            Outcome::failed()
        );
        // Cancel-ops classifier: only abort escalates.
        assert!(StopConditionDecl::action_cancels_ops(Some("abort")));
        assert!(!StopConditionDecl::action_cancels_ops(Some("fail")));
        assert!(!StopConditionDecl::action_cancels_ops(Some("stop")));
        assert!(!StopConditionDecl::action_cancels_ops(None));

        // A compiled abort decl surfaces cancel_ops=true through evaluate.
        let root = polydat::dsl::compile_polydat("input cycle: u64").expect("root kernel");
        let mut set = StopConditionSet::build_for_phase(
            &root,
            &[StopConditionDecl {
                when: "result_failure > 0".to_string(),
                effect: Outcome::failed(),
                reason: Some("terminal_failure".to_string()),
                target: StopScope::Workload,
                cancel_ops: true,
            }],
        )
        .expect("build set");
        assert_eq!(
            set.evaluate(&RuntimeState {
                result_failure: 1,
                ..Default::default()
            }),
            Some((
                Outcome::failed(),
                "terminal_failure".to_string(),
                StopScope::Workload,
                true
            ))
        );
    }
}
