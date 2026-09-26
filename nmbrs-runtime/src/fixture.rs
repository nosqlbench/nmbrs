// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Init-time scope fixture and cycle-time pull plan.
//!
//! Implements SRD 32 §"Init-Time Fixture and Consumer Self-
//! Registration". Each op-template consumer (validation,
//! conditional, throttle, …) registers the Polydat names it will read
//! at cycle time into a shared [`ScopeFixture`]. The fixture is
//! sealed once construction completes and yields a [`PullPlan`]
//! whose entries are materialized per cycle into a [`ResolvedPulls`]
//! buffer indexed by the [`PullHandle`]s the consumer received at
//! registration.
//!
//! This is the **only** path by which wrappers gain access to GK
//! values at cycle time (SRD 31 §"Pull plan vs bind plan"). The
//! prior side channel — wrapper names threaded through
//! `synthesis::resolve_with_extras` into `ResolvedFields` — has
//! been removed; the resolver now exposes `resolve_with_field_pulls`,
//! which carries op-field bind points only (the adapter-facing
//! read).

use std::collections::HashMap;
use std::sync::Arc;

use crate::scope_kernel::ScopeKernel;
use polydat::ast::Value;
use polydat::kernel::PolydatProgram;

/// What the kernel reports a registered name resolves to.
///
/// Captures live in `input_defs` (the input array) at indices
/// `>= coord_count`, so they share the `Input` variant — no
/// separate capture variant is needed.
#[derive(Debug, Clone)]
enum PlanEntry {
    Output { name: String, output_idx: usize },
    Input { name: String, input_idx: usize },
}

impl PlanEntry {
    fn name(&self) -> &str {
        match self {
            PlanEntry::Output { name, .. } => name,
            PlanEntry::Input { name, .. } => name,
        }
    }
}

/// Init-time accumulator for consumer-declared pulls.
///
/// Holds an `Arc<PolydatProgram>` clone of the per-template canonical
/// kernel's program (SRD 16 §"Per-Scope Canonical Kernel Cache").
/// The program carries every fact this fixture needs at
/// registration time — the output map, input definitions, and
/// auto-extern-provisioned externs — so no per-fiber state is
/// touched until the sealed [`PullPlan`] is resolved at cycle
/// time. Each consumer registers names via [`register_pull`];
/// the fixture deduplicates by name and yields a stable
/// [`PullHandle`] for each unique registration.
///
/// [`register_pull`]: ScopeFixture::register_pull
pub struct ScopeFixture {
    program: Arc<PolydatProgram>,
    handles: HashMap<String, PullHandle>,
    plan: Vec<PlanEntry>,
}

impl ScopeFixture {
    /// Open a new fixture against a per-template canonical
    /// program. The Arc clone is cheap; the activity construction
    /// loop typically already holds the program for adapter
    /// dispatch and bind-plan synthesis.
    pub fn new(program: Arc<PolydatProgram>) -> Self {
        Self {
            program,
            handles: HashMap::new(),
            plan: Vec::new(),
        }
    }

    /// The program this fixture is scoped against. Useful for
    /// consumers that need to inspect the manifest (e.g. type-
    /// aware strict parsing).
    pub fn program(&self) -> &Arc<PolydatProgram> {
        &self.program
    }

    /// Register a name. Resolves it against the program's output
    /// map (folded constants) first, then against `find_input`
    /// (extern slots, capture inputs, coordinates).
    ///
    /// Returns a memoized handle. Re-registering the same name
    /// returns the existing handle — registrations are idempotent.
    ///
    /// **Errors** when the program does not know the name. The
    /// Polydat compiler is responsible for provisioning every name
    /// referenced anywhere in the op template (op fields and
    /// params; SRD 16 §"Auto-Extern Generation"). An unknown
    /// name here therefore signals a workload bug — typically a
    /// reference to a binding that was never declared, or a
    /// typo that the compiler should have already caught.
    pub fn register_pull(&mut self, name: &str) -> Result<PullHandle, String> {
        if let Some(&h) = self.handles.get(name) {
            return Ok(h);
        }
        let entry = if let Some((node_idx, _port_idx)) = self.program.resolve_output(name) {
            // Output: prefer the output index over (node_idx, port_idx)
            // because state.pull_by_index resolves the pair internally
            // and matches OutputAccessor's existing pattern.
            let output_idx = self.program.output_index(name).ok_or_else(|| {
                format!(
                    "fixture: name '{name}' resolved to output node {node_idx} \
                     but had no entry in the output_list — this is a kernel \
                     consistency bug, please report",
                )
            })?;
            PlanEntry::Output {
                name: name.to_string(),
                output_idx,
            }
        } else if let Some(input_idx) = self.program.find_input(name) {
            PlanEntry::Input {
                name: name.to_string(),
                input_idx,
            }
        } else {
            return Err(format!(
                "fixture: name '{name}' is not known to the program — neither \
                 a declared output nor an input slot. The Polydat compiler should \
                 have provisioned it from a bind-point reference somewhere in \
                 the op template; if it didn't, the workload is referencing a \
                 binding that doesn't exist. Available outputs: [{outs}]; \
                 inputs: [{ins}].",
                outs = self.program.output_names().join(", "),
                ins = self.program.input_names().join(", "),
            ));
        };
        let handle = PullHandle(self.plan.len());
        self.plan.push(entry);
        self.handles.insert(name.to_string(), handle);
        Ok(handle)
    }

    /// Seal and yield the immutable plan. The plan owns its own
    /// `Arc<PolydatProgram>` clone, so cycle-time `resolve` only needs
    /// the per-fiber state.
    pub fn seal(self) -> PullPlan {
        PullPlan {
            program: self.program,
            entries: self.plan,
        }
    }
}

/// Opaque, copy-able handle into a [`PullPlan`]. The only way to
/// turn a handle into a value is [`ResolvedPulls::get`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct PullHandle(usize);

impl PullHandle {
    /// Internal: index into the plan / resolved buffer. Public
    /// only to the crate so test scaffolding can build synthetic
    /// pulls; outside this crate, handles are opaque.
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

/// Sealed init-time plan. One entry per unique name registered.
/// Owns an `Arc<PolydatProgram>` so cycle-time resolve only needs the
/// per-fiber state.
pub struct PullPlan {
    program: Arc<PolydatProgram>,
    entries: Vec<PlanEntry>,
}

impl PullPlan {
    /// Assert this plan is being resolved against the program it was BUILT
    /// against.
    ///
    /// A plan holds resolved indices, not names — `Output { output_idx: 7 }`.
    /// Resolving it against a different program silently reindexes every read:
    /// where the index happens to be in range it returns a NEIGHBOURING wire's
    /// value (a different metric, reported under this one's name), and where it
    /// is out of range it panics deep in the engine as
    /// "index out of bounds", naming nothing that points back here.
    ///
    /// Both happened: a CQL dispenser that returned `None` from
    /// `canonical_kernel()` left its per-op slot empty, so plans built against
    /// the op-template program fell back to the phase kernel. One metric read
    /// its phase's `__metric_time_to_index`; the next two panicked.
    ///
    /// The check is an `Arc` pointer comparison per op per cycle — cheaper than
    /// the first index it protects.
    pub(crate) fn check_program_match(&self, program: &Arc<PolydatProgram>, template_idx: usize) {
        if Arc::ptr_eq(&self.program, program) {
            return;
        }
        panic!(
            "pull plan for op #{template_idx} was built against a different \
             program than the kernel it is being resolved against — every \
             index in it addresses the wrong wire.\n  \
             plan program outputs:   {:?}\n  \
             kernel program outputs: {:?}\n\
             This is a wiring bug, not a workload error. The usual cause is a \
             dispenser whose `canonical_kernel()` returns `None` (so no per-op \
             kernel is materialised and the plan falls back to the phase \
             kernel) — every leaf dispenser must return the op-template kernel \
             it was mapped against.",
            self.program.output_names(),
            program.output_names(),
        );
    }

    /// Number of distinct names in this plan.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this plan has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The names in the plan, in registration order. Diagnostic only.
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name()).collect()
    }

    /// The program this plan was sealed against.
    pub fn program(&self) -> &Arc<PolydatProgram> {
        &self.program
    }

    /// Materialize every entry against `kernel`, a kernel of this
    /// plan's program on any engine (its indices are the program's).
    /// Output entries go through `pull_at` (eval cone if not current);
    /// input entries through `input_value_at` (cell-aware for shared
    /// slots).
    ///
    /// O(plan_len) on the hot path — no name hashing.
    pub fn resolve(&self, kernel: &mut dyn polydat::Kernel) -> ResolvedPulls {
        let mut values = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let v = match entry {
                PlanEntry::Output { output_idx, .. } => kernel.pull_at(*output_idx),
                PlanEntry::Input { input_idx, .. } => {
                    kernel.input_value_at(*input_idx).unwrap_or(Value::None)
                }
            };
            values.push(v);
        }
        ResolvedPulls { values }
    }

    /// Convenience: resolve against a scope kernel. Provided so
    /// test scaffolding and other ergonomic call sites don't have to
    /// name the trait object.
    pub fn resolve_with(&self, kernel: &mut ScopeKernel) -> ResolvedPulls {
        self.resolve(kernel.kernel_mut())
    }
}

/// Cycle-time materialization of a [`PullPlan`].
///
/// Read-only, indexed by [`PullHandle`]. Created once per cycle
/// during the resolve phase (SRD 31 §"Cycle-Time Pipeline"); the
/// values inside reflect the PolydatState snapshot at the moment of
/// resolution and are not invalidated by subsequent state changes.
pub struct ResolvedPulls {
    values: Vec<Value>,
}

impl ResolvedPulls {
    /// Empty pulls — no consumer registered anything. Used as the
    /// default in the wrapper construction path before α.4 deletes
    /// the legacy `extras` flow.
    pub fn empty() -> Self {
        Self { values: Vec::new() }
    }

    /// Resolve a handle to a borrowed value. Panics on a handle
    /// from a different plan (the index is out of range). This is
    /// a programming error — a handle's plan provenance is
    /// statically associated with the wrapper that owns it, and
    /// `ExecCtx` carries the matching `ResolvedPulls`.
    pub fn get(&self, h: PullHandle) -> &Value {
        &self.values[h.index()]
    }

    /// Number of resolved values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether this resolution is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Trait every cross-cutting wrapper that reads Polydat values must
/// implement. The activity construction loop calls `fixture` once
/// per template per consumer; failures (closed-vocab violation,
/// missing required field, unresolvable name) are returned as
/// `Err` and abort construction loudly.
pub trait OpConsumer: Sized {
    /// Inspect `template`, register the names this consumer will
    /// read at cycle time, and return a fully-configured instance
    /// holding handles plus parsed (strict) config.
    fn fixture(
        template: &nmbrs_workload::model::ParsedOp,
        fx: &mut ScopeFixture,
    ) -> Result<Self, String>;
}

/// Cycle-time bundle handed to every dispenser via
/// `OpDispenser::execute`. Adapters use `fields` exclusively;
/// wrappers read `pulls` via stored handles.
///
/// Bundling rather than passing two parameters keeps the trait
/// surface forward-compatible for per-cycle component context,
/// streaming-capture hooks, and diagnostic taps (SRD 32
/// §"`ExecCtx` — cycle-time bundle").
pub struct ExecCtx<'a> {
    pub fields: &'a crate::adapter::ResolvedFields,
    pub pulls: &'a ResolvedPulls,
    /// Narrow read surface for op-template name resolution against
    /// the dispenser's bound Polydat context (SRD-68 invariants I-1 + I-2).
    /// During the SRD-68 migration this defaults to a no-op
    /// `NullWireSource` for legacy call sites; adapters that own a
    /// kernel construct via [`Self::with_wires`].
    pub wires: &'a dyn crate::wires::WireSource,
    /// Number of consecutive wire ordinals this invocation should
    /// cover — the ACTUAL length of the cursor sub-run the executor
    /// reserved for this op (`base .. actual_end`). Equals the op's
    /// [`crate::adapter::OpDispenser::rows_per_op`] except at the
    /// cursor tail, where the final reservation is short: a batch op
    /// reads exactly `[cycle, cycle + run_len)` so the partial tail
    /// of `M % N` rows is inserted too — never over-read, never
    /// dropped. Ordinary (non-batch) ops ignore it. Defaults to `1`
    /// for every legacy / test call site (single-row semantics).
    pub run_len: usize,
}

impl<'a> ExecCtx<'a> {
    /// Legacy constructor — defaults `wires` to a no-op
    /// [`crate::wires::NullWireSource`]. Used by call sites that
    /// haven't migrated to SRD-68's dispenser-owned-kernel model
    /// yet. Adapters that own a kernel handle should call
    /// [`Self::with_wires`] instead.
    pub fn new(fields: &'a crate::adapter::ResolvedFields, pulls: &'a ResolvedPulls) -> Self {
        Self {
            fields,
            pulls,
            wires: &crate::wires::NULL_WIRES,
            run_len: 1,
        }
    }

    /// Construct an `ExecCtx` with an explicit `WireSource` — the
    /// SRD-68 path. The `wires` value should be the per-fiber
    /// kernel slot for the firing dispenser, narrowed to the
    /// `WireSource` trait so adapter code never sees `ScopeKernel`
    /// internals.
    pub fn with_wires(
        fields: &'a crate::adapter::ResolvedFields,
        pulls: &'a ResolvedPulls,
        wires: &'a dyn crate::wires::WireSource,
    ) -> Self {
        Self {
            fields,
            pulls,
            wires,
            run_len: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k() -> ScopeKernel {
        crate::bindings::compile_scope_kernel(
            "input cycle: u64\n\
             folded := 42\n\
             cyc_dep := hash(cycle)\n",
            &Default::default(),
        )
        .expect("compile_scope_kernel")
    }

    #[test]
    fn register_resolves_folded_output() {
        let kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let h = fx.register_pull("folded").expect("folded should resolve");
        let plan = fx.seal();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.names(), vec!["folded"]);
        let _ = h; // handle is opaque from outside
    }

    #[test]
    fn register_resolves_cycle_dependent_output() {
        let kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        fx.register_pull("cyc_dep").expect("cyc_dep should resolve");
        let plan = fx.seal();
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn register_resolves_input() {
        let kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        // 'cycle' is the coordinate input, not an output.
        fx.register_pull("cycle")
            .expect("cycle input should resolve");
        let plan = fx.seal();
        assert_eq!(plan.names(), vec!["cycle"]);
    }

    #[test]
    fn register_unknown_name_errors() {
        let kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let err = fx.register_pull("nonexistent").unwrap_err();
        assert!(
            err.contains("nonexistent"),
            "error should name the missing binding: {err}"
        );
        assert!(
            err.contains("Available outputs"),
            "error should list available outputs: {err}"
        );
    }

    #[test]
    fn register_is_idempotent_per_name() {
        let kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let h1 = fx.register_pull("folded").unwrap();
        let h2 = fx.register_pull("folded").unwrap();
        assert_eq!(h1, h2, "same name should yield same handle");
        let plan = fx.seal();
        assert_eq!(
            plan.len(),
            1,
            "duplicate registrations should not grow the plan"
        );
    }

    #[test]
    fn register_assigns_distinct_handles_for_distinct_names() {
        let kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let h_folded = fx.register_pull("folded").unwrap();
        let h_cyc = fx.register_pull("cyc_dep").unwrap();
        assert_ne!(h_folded, h_cyc);
        let plan = fx.seal();
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn resolve_pulls_folded_output_value() {
        let mut kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let h = fx.register_pull("folded").unwrap();
        let plan = fx.seal();

        kernel.set_inputs(&[0]);
        let pulls = plan.resolve_with(&mut kernel);
        let v = pulls.get(h);
        assert_eq!(v.as_u64(), 42);
    }

    #[test]
    fn resolve_pulls_cycle_dependent_value_per_cycle() {
        let mut kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let h = fx.register_pull("cyc_dep").unwrap();
        let plan = fx.seal();

        kernel.set_inputs(&[0]);
        let v0 = plan.resolve_with(&mut kernel).get(h).as_u64();
        kernel.set_inputs(&[1]);
        let v1 = plan.resolve_with(&mut kernel).get(h).as_u64();
        assert_ne!(v0, v1, "cycle-dependent output should change per cycle");
    }

    #[test]
    fn resolve_pulls_input_slot_value() {
        let mut kernel = k();
        let mut fx = ScopeFixture::new(kernel.program().clone());
        let h = fx.register_pull("cycle").unwrap();
        let plan = fx.seal();

        kernel.set_inputs(&[7]);
        let pulls = plan.resolve_with(&mut kernel);
        assert_eq!(pulls.get(h).as_u64(), 7);
    }

    #[test]
    fn empty_plan_resolves_to_empty_pulls() {
        let mut kernel = k();
        let fx = ScopeFixture::new(kernel.program().clone());
        let plan = fx.seal();
        kernel.set_inputs(&[0]);
        let pulls = plan.resolve_with(&mut kernel);
        assert!(pulls.is_empty());
    }
}
