// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-fiber Polydat Kernel construction.
//!
//! `OpBuilder` owns the activity's source kernel and seeds each
//! per-fiber [`FiberBuilder`] with a typed subscope plus any
//! captured init-binding values and named scope overrides. The
//! adapter-facing cycle-time bind-point resolution path
//! historically lived here too, but SRD-68 Push 5 retired it in
//! favour of the generic [`crate::wires::WireSource`] surface;
//! this module now scopes to fiber construction and bind-point
//! validation.
//!
//! See `docs/SRD/68_dispenser_owned_polydat_context.md` for the
//! resolution model.

use std::sync::{Arc, OnceLock};

use nmbrs_workload::bindpoints::{self, BindPoint, BindQualifier};
use nmbrs_workload::model::ParsedOp;
use polydat::ast::Value;
use polydat::kernel::{PolydatKernel, PolydatProgram, PolydatState};

/// Cached `NMBRS_DIRTY_DEBUG` flag — per-cycle `std::env::var`
/// reads measured at ~30% of single-fiber CPU; the OnceLock
/// makes the gate a single atomic load on the hot path. See
/// the matching helpers in `wires.rs` / polydat's `engines.rs`.
fn nmbrs_dirty_debug_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("NMBRS_DIRTY_DEBUG").is_ok())
}

/// Shared op builder that distributes per-fiber builders.
///
/// Holds the `Arc<PolydatProgram>` (immutable, shared). Each executor
/// fiber calls `create_fiber_builder()` to get its own `FiberBuilder`
/// with private `PolydatState` — no locks, no contention on the hot path.
pub struct OpBuilder {
    /// Values to inject into every new FiberBuilder's state at creation.
    /// Used for scope composition: outer scope constants are set as
    /// initial values for inner scope extern inputs.
    /// Name-keyed scope values (per SRD-13c). Stored by name
    /// rather than `(input_idx, value)` because each kernel —
    /// the fiber main kernel, every per-op-template kernel —
    /// owns its own input layout, and an index captured against
    /// the source kernel doesn't translate. The previous
    /// `Vec<(usize, Value)>` shape silently mis-routed writes
    /// across kernels (e.g. `table` value landing in the
    /// `profile` slot of an op-template kernel whose extern
    /// declaration order differed from the phase scope).
    scope_values: Vec<(String, Value)>,
    /// Pre-evaluated init binding values to seed into every new
    /// FiberBuilder's state — `(node_idx, port_idx, value)`. Captured
    /// from the activation kernel after [SRD 11](../../docs/SRD/11_polydat_evaluation.md)
    /// §"Init Binding Contract" Plan B has run. With this seeding,
    /// the binding's eval function fires exactly once per scope
    /// activation (on the activation kernel), not once per fiber.
    init_overrides: Vec<(usize, usize, Value)>,
    /// The source kernel — the activity's own kernel that
    /// each per-fiber `FiberBuilder` materializes a subscope
    /// of via [`PolydatKernel::materialize_subscope`]. Owning the
    /// kernel (not just its program) carries the activity's
    /// full cell state — own input-slot cells plus transit
    /// cells inherited from ancestors — to every fiber's main
    /// kernel via the typed subscope protocol.
    ///
    /// Routes per-fiber kernel construction through the only
    /// two sanctioned paths: root (`compile_polydat`) or
    /// parent-supervised subscope (`materialize_subscope`).
    /// The earlier `instance_program(program)` path is
    /// removed — it produced parentless kernels that lost
    /// the cell handles workload-root → fiber needs.
    source_kernel: Arc<PolydatKernel>,
    /// SRD-13d Phase 9 — per-op-template kernel programs keyed
    /// by op name. Populated by [`Self::with_op_template_programs`]
    /// when the runner has materialised op-template kernels in
    /// the scope tree. Wrappers (e.g. `MetricsDispenser`) look up
    /// the program for their template via [`Self::program_for_op`]
    /// and build their `ScopeFixture` against it; flattened
    /// op-templates fall through to the activity-wide `program`.
    op_template_programs: std::collections::HashMap<String, Arc<PolydatProgram>>,
}

impl OpBuilder {
    /// Create an OpBuilder from a kernel.
    /// If the kernel has scope values (set via `materialize_wiring_from_outer`
    /// or directly via `kernel.state().set_input`), they are
    /// captured and propagated into every fiber's state.
    ///
    /// Init binding values that have been pulled on the kernel's
    /// state (typically by the scope-init pass right after
    /// `materialize_wiring_from_outer`) are likewise captured as
    /// [`Self::init_overrides`] and propagated to every fiber, so
    /// init eval fires once per activation rather than once per
    /// fiber.
    pub fn new(kernel: impl Into<Arc<PolydatKernel>>) -> Self {
        let kernel: Arc<PolydatKernel> = kernel.into();
        // Scope values seed PLAIN slots only. A CELL-BOUND slot's value
        // is the live shared cell — snapshotting it here freezes the
        // cell's activity-start value, and every downstream
        // re-application (fiber seeding, the stanza-boundary
        // `reset_captures`) would `set_input` that stale snapshot
        // THROUGH the cell, clobbering later writes for every kernel
        // sharing it. Same exclusion `reset_inputs_from` documents:
        // cells are cross-kernel shared state with their own
        // lifecycle.
        let scope_values: Vec<(String, Value)> = kernel
            .scope_values()
            .into_iter()
            .filter(|(name, _)| {
                kernel
                    .program()
                    .find_input(name)
                    .map(|idx| kernel.state_ref().shared_cell(idx).is_none())
                    .unwrap_or(true)
            })
            .collect();
        let init_overrides = collect_init_overrides(&kernel);
        Self {
            scope_values,
            init_overrides,
            source_kernel: kernel,
            op_template_programs: std::collections::HashMap::new(),
        }
    }

    /// Install per-op-template kernel programs (SRD-13d Phase 9).
    /// The runner builds these from the scope tree's
    /// `cached_kernel` slots for materialised op-template scopes
    /// and threads them here so wrappers can look up the right
    /// program when constructing their fixtures.
    pub fn with_op_template_programs(
        mut self,
        programs: std::collections::HashMap<String, Arc<PolydatProgram>>,
    ) -> Self {
        self.op_template_programs = programs;
        self
    }

    /// Look up the kernel program for op `name`. Returns the
    /// per-op-template program if Phase 9 produced one for this
    /// op (i.e. `materialised` and bindings non-empty); otherwise
    /// returns the activity-wide program (the flatten path).
    pub fn program_for_op(&self, name: &str) -> Arc<PolydatProgram> {
        self.op_template_programs
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.source_kernel.program().clone())
    }

    /// The activity-wide kernel program. Used by callers that
    /// need the source program shape (output names, manifest)
    /// without rebuilding a fresh kernel.
    pub fn program(&self) -> Arc<PolydatProgram> {
        self.source_kernel.program().clone()
    }

    /// The activity-wide source kernel — the Polydat context every
    /// op-template subscope is built upon. Adapters' `map_op`
    /// implementations receive a clone of this Arc as the `parent`
    /// argument so they can materialise their own canonical
    /// op-template kernel via SRD-67 `build_subscope` (SRD-68
    /// invariant I-3) or simply retain the Arc when their op has
    /// no matter to add.
    pub fn source_kernel(&self) -> &Arc<PolydatKernel> {
        &self.source_kernel
    }

    /// Build the canonical op-template kernel for `op_name` —
    /// the Polydat context the dispenser owns and that per-fiber
    /// instances are materialised from (SRD-68 invariants I-3,
    /// I-4). Equivalent in shape to today's per-fiber
    /// `op_template_kernels[op_name]` but built once at dispenser
    /// construction time, lifted out of the per-fiber HashMap.
    ///
    /// When `op_name` has a registered op-template program (phase
    /// `bindings:`, op-level `bindings:`, `result:` block — the
    /// matter assembled by the synthesis pipeline before the
    /// activity runs), the canonical is a fresh subscope of
    /// `source_kernel` carrying that program. Otherwise the
    /// canonical is the source kernel itself (Arc-cloned), which
    /// covers the flattened-op-template path (no per-op matter).
    pub fn canonical_kernel_for_op(&self, op_name: &str) -> Arc<PolydatKernel> {
        match self.op_template_programs.get(op_name) {
            Some(program) => {
                let canonical = self
                    .source_kernel
                    .build_subscope(
                        polydat::kernel::subcontext::PolydatMatter::builder()
                            .program(program.clone())
                            .build()
                            .expect("program-form matter is infallible"),
                    )
                    .expect("program-form subscope is infallible");
                Arc::new(canonical)
            }
            None => self.source_kernel.clone(),
        }
    }

    /// Create a per-fiber builder. No locks, no sharing — the fiber
    /// owns its state exclusively. Scope values (per-iteration
    /// inputs from `for_each` / `for_combinations` / outer scope
    /// constants) are injected into the state's extern inputs and
    /// remembered on the builder so `reset_captures` (called at
    /// stanza boundaries) can re-apply them — otherwise the
    /// blanket "reset all non-coord inputs" pass would clobber
    /// the iteration's bound values.
    ///
    /// Init binding values captured by the scope-init pass are
    /// seeded into the fiber's node buffers and the corresponding
    /// nodes are marked clean. This is the runtime mechanism
    /// behind the init-binding contract: each fiber's first pull
    /// of an init binding reads the pre-evaluated value directly
    /// instead of running the binding's eval.
    pub fn create_fiber_builder(&self) -> FiberBuilder {
        // The fiber's main kernel is a typed subscope of the
        // activity's source kernel — built via the only
        // sanctioned subscope construction path. Cells, transit
        // cells, and value-copy bindings flow in automatically;
        // the fiber observes the same cell handles as the
        // workload-root through the chain.
        let mut fb = FiberBuilder::new(&self.source_kernel);
        // Install scope_values + precompute each value's input
        // index against `main_kernel`. The cached indices feed
        // the per-cycle `reset_captures` path (replacing the
        // linear `find_input` scan that used to dominate the
        // single-fiber dryrun=cycle profile at ~11% self time).
        fb.set_scope_values(self.scope_values.clone());
        // Apply scope values to the main kernel through the
        // typed Dataflow surface so type-mismatched scope values
        // fail loud rather than silently corrupting downstream
        // reads. Walks the pre-resolved index list — no second
        // `find_input` call.
        for ((name, value), idx_opt) in fb.scope_values.iter().zip(fb.scope_value_main_idx.iter()) {
            if let Some(idx) = idx_opt {
                crate::wires::write_input(&mut fb.main_kernel, *idx, name, value.clone())
                    .unwrap_or_else(|e| {
                        panic!("scope value '{name}' failed typed write at scope-init: {e}")
                    });
            }
        }
        for (node_idx, port_idx, value) in &self.init_overrides {
            fb.state()
                .seed_node_buffer(*node_idx, *port_idx, value.clone());
        }
        // SRD-68: per-fiber op-template kernels are populated by
        // `attach_dispenser_kernels`, which runs right after this
        // function returns (see executor cycle dispatch). The
        // legacy `op_template_kernels` HashMap and its name-keyed
        // population have retired; per-fiber instances now live in
        // `per_op_kernels: Vec<Option<PolydatKernel>>` indexed parallel
        // to the dispenser registry, with each adapter's
        // `OpDispenser::canonical_kernel()` providing the per-op
        // canonical that gets `build_subscope`-instanced per fiber.
        fb
    }
}

/// Read each init-output's pulled value off the kernel's state,
/// returning `(node_idx, port_idx, value)` triples suitable for
/// seeding fiber states. `Value::None` entries are skipped — the
/// scope-init pass should have errored on those before getting
/// here, but defensively we don't propagate them either way.
fn collect_init_overrides(kernel: &PolydatKernel) -> Vec<(usize, usize, Value)> {
    let program = kernel.program();
    let init_outputs = program.const_outputs();
    if init_outputs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(init_outputs.len());
    let state = kernel.state_ref();
    for name in init_outputs {
        let Some(&(node_idx, port_idx)) = program.output_map_lookup(name) else {
            continue;
        };
        match state.node_buffer(node_idx, port_idx) {
            Some(v) if !matches!(v, Value::None) => {
                out.push((node_idx, port_idx, v.clone()));
            }
            _ => {} // None / out-of-range — Plan B should have caught it
        }
    }
    out
}

/// The input a scope value is written to on `program`, or `None` when
/// the program has no such input or it is a coordinate. A coordinate
/// (the `cycle` a scope carries among its values) advances with every
/// cycle through `set_inputs` and is never written by name; the stanza
/// reset leaves it untouched, so there is nothing to re-apply.
fn scope_value_index(program: &PolydatProgram, name: &str) -> Option<usize> {
    let idx = program.find_input(name)?;
    (program.input_kind(idx) != Some(polydat::kernel::InputKind::Coordinate)).then_some(idx)
}

/// Per-fiber op builder. Owns its own PolydatState.
/// No locks, no synchronization, no contention.
///
/// Created via `OpBuilder::create_fiber_builder()` at fiber startup.
///
/// When capture extraction is implemented, captured values will
/// write directly to Polydat volatile/sticky ports on the state,
/// bypassing any intermediate storage.
pub struct FiberBuilder {
    /// The fiber's main kernel — typically the activity-wide
    /// (workload / phase) program. State lives inside the
    /// kernel; access via [`Self::state`] and
    /// [`Self::state_ref`].
    main_kernel: PolydatKernel,
    /// Scope-bound input values (per-iteration extern bindings)
    /// that should persist across stanza-level
    /// `reset_inputs_from` resets. Empty for a builder created
    /// via plain [`FiberBuilder::new`]; populated by
    /// [`OpBuilder::create_fiber_builder`].
    scope_values: Vec<(String, Value)>,
    /// SRD-68 invariant I-4 — per-fiber kernel instances, indexed
    /// parallel to the activity's dispenser registry. Each entry
    /// is a `build_subscope` materialisation of the corresponding
    /// dispenser's canonical kernel (the kernel the dispenser owns
    /// per SRD-68 I-3); `None` for dispensers that don't expose
    /// a canonical kernel (adapters with no Polydat needs, or wrappers
    /// that delegate). Populated by
    /// [`Self::attach_dispenser_kernels`] right after fiber spawn,
    /// before any cycles run; read at cycle dispatch to populate
    /// `ExecCtx::wires` for the firing dispenser.
    per_op_kernels: Vec<Option<PolydatKernel>>,
    /// Per-op-kernel side-effecting output names (parallels
    /// [`Self::per_op_kernels`]). The subset of each op-template
    /// kernel's outputs whose cone contains a `Purity::SideChannel`
    /// node — the only outputs the per-cycle "fire side effects" pass
    /// pulls. Computed once at [`Self::attach_dispenser_kernels`] so
    /// volatile metric-reader outputs (the objective bindings) are NOT
    /// re-evaluated every cycle just to fire a non-existent effect.
    per_op_side_effecting: Vec<Vec<String>>,
    /// Pre-resolved input indices for each entry in
    /// [`Self::scope_values`] against [`Self::main_kernel`].
    /// `None` slots are scope values the main program doesn't
    /// declare (silently skipped at write time — matches the
    /// historical name-based-skip semantics).
    ///
    /// Eliminates the `PolydatProgram::find_input` linear scan that
    /// fired per scope-value per cycle in `reset_captures`.
    /// Populated once at [`Self::new`] (initially empty since
    /// scope_values is empty) and refreshed by
    /// [`Self::set_scope_values`] when the OpBuilder seeds them.
    scope_value_main_idx: Vec<Option<usize>>,
    /// Per-op-kernel mirror of [`Self::scope_value_main_idx`].
    /// Outer Vec parallels [`Self::per_op_kernels`]; inner Vec
    /// parallels [`Self::scope_values`]. `None` outer slots
    /// match per_op_kernels' `None` entries (no canonical kernel
    /// for that dispenser). Built inside
    /// [`Self::attach_dispenser_kernels`] after each subscope is
    /// constructed so the per-cycle reset path becomes pure
    /// indexed `set_input`.
    scope_value_per_op_idx: Vec<Option<Vec<Option<usize>>>>,
}

/// Validate that all bind points in op templates can be resolved.
///
/// Called at init time. Warns for each unresolvable `{name}` reference.
/// A bind point is resolvable if it matches a Polydat output, input name,
/// or a known capture declaration from another op. Workload params are
/// injected into the Polydat source as constant bindings before compilation,
/// so they resolve as Polydat outputs.
/// Validate that all bind points in op templates can be resolved.
///
/// Returns `Err` with a descriptive message if any bind point is
/// unresolvable. Callers should treat this as a fatal error —
/// unresolved bind points produce broken ops at runtime.
pub fn validate_bind_points(
    templates: &[ParsedOp],
    program_for_op: &dyn Fn(&str) -> Arc<PolydatProgram>,
) -> Result<(), String> {
    // Collect all capture declarations across templates. Captures
    // are extracted at workload-parse time and live on
    // `ParsedOp.captures`; the op-text fields have brackets
    // stripped by then, so re-parsing the text wouldn't surface
    // them.
    let mut capture_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for template in templates {
        for cap in &template.captures {
            capture_names.insert(cap.as_name.clone());
        }
    }

    let mut errors: Vec<String> = Vec::new();

    for template in templates {
        // THIS op's program, not the activity-wide one. An op that owns a
        // kernel (its own `bindings:`, or an adapter that materialises one)
        // has its bindings in that program and nowhere else, so validating
        // every template against the activity kernel reported an op's own
        // binding as unresolvable — `{x}` in `stmt:`/`raw:` failed at RUNTIME
        // while the same name still rendered fine in `memo:`/`gutter:`, which
        // read the live wires instead of this check.
        let program = program_for_op(&template.name);
        for (field_name, value) in &template.op {
            if let serde_json::Value::String(s) = value {
                let bps = bindpoints::extract_bind_points(s);
                for bp in &bps {
                    if let BindPoint::Reference {
                        name, qualifier, ..
                    } = bp
                    {
                        let resolvable = match qualifier {
                            BindQualifier::Bind => program.resolve_output(name).is_some(),
                            BindQualifier::Capture => capture_names.contains(name),
                            BindQualifier::Input => {
                                program.input_names().contains(&name.to_string())
                            }
                            BindQualifier::None => {
                                program.resolve_output(name).is_some()
                                    || capture_names.contains(name)
                                    || program.input_names().contains(&name.to_string())
                            }
                        };
                        if !resolvable {
                            errors.push(format!(
                                "unresolved bind point '{{{name}}}' in op '{}' field '{field_name}'. \
                                 Not found in Polydat bindings, captures, or inputs.",
                                template.name
                            ));
                        }
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        for e in &errors {
            crate::observer::log(crate::observer::LogLevel::Error, &format!("error: {e}"));
        }
        Err(format!("{} unresolved bind point(s)", errors.len()))
    }
}

impl FiberBuilder {
    /// Create a new fiber builder as a typed subscope of the
    /// activity's source kernel.
    ///
    /// The fiber's main kernel is built via
    /// [`PolydatKernel::materialize_subscope`] — the typed parent →
    /// child construction path. Per-fiber state is fresh; cell
    /// handles are Arc-shared with the parent so writes
    /// propagate to the workload-root through the cascade.
    pub fn new(parent: &PolydatKernel) -> Self {
        let main_kernel = parent
            .build_subscope(
                polydat::kernel::subcontext::PolydatMatter::builder()
                    .program(parent.program().clone())
                    .build()
                    .unwrap(),
            )
            .expect("program-form subscope is infallible");
        Self {
            main_kernel,
            scope_values: Vec::new(),
            per_op_kernels: Vec::new(),
            per_op_side_effecting: Vec::new(),
            scope_value_main_idx: Vec::new(),
            scope_value_per_op_idx: Vec::new(),
        }
    }

    /// Install the scope-value list and precompute each value's
    /// input index against `main_kernel`. Indices are reused on
    /// the per-cycle [`Self::reset_captures`] path so the
    /// historical per-scope-value `find_input` linear scan is
    /// retired.
    ///
    /// Called once by [`OpBuilder::create_fiber_builder`] right
    /// after the fiber's main kernel is built — `scope_values`
    /// is otherwise immutable for the fiber lifetime.
    fn set_scope_values(&mut self, scope_values: Vec<(String, Value)>) {
        let program = self.main_kernel.program();
        self.scope_value_main_idx = scope_values
            .iter()
            .map(|(name, _)| scope_value_index(program, name))
            .collect();
        self.scope_values = scope_values;
    }

    /// SRD-68 Push 3 — populate this fiber's per-op kernel slots
    /// from the activity's dispenser registry. Walks each
    /// dispenser, calls `dispenser.canonical_kernel()` to get the
    /// dispenser-owned canonical kernel (when present), and
    /// materialises a per-fiber subscope kernel via
    /// `build_subscope`. Slot positions match the dispenser
    /// registry's order so cycle-time dispatch can index by
    /// `template_idx`. Dispensers that return `None` (no GK
    /// needs) get a `None` slot — `ExecCtx::wires` falls back to
    /// the `NullWireSource` baseline for those cycles.
    ///
    /// Called once per fiber, right after spawn, before any cycles
    /// run. Idempotent: re-attaching with the same registry is
    /// a no-op since canonical kernels are stable across phase
    /// activation.
    pub fn attach_dispenser_kernels(
        &mut self,
        dispensers: &[std::sync::Arc<dyn crate::adapter::OpDispenser>],
    ) {
        let scope_values = self.scope_values.clone();
        // SRD-13f Stage 1: per-op kernels descend from
        // `fiber.main_kernel` (this fiber's per-fiber scope
        // kernel for the current phase), NOT from the
        // dispenser's shared `canonical_kernel`. The dispenser's
        // canonical_kernel becomes a *program source* — we
        // extract its program shape and build the per-op
        // kernel as a per-fiber subscope of main_kernel. This
        // collapses what used to be two parallel kernel
        // lineages (`source_kernel → fiber.main_kernel` and
        // `source_kernel → canonical → per_op`) into one
        // consistent per-fiber chain:
        //     fiber.main_kernel → per_op_kernel
        // Computed outputs on main_kernel are reachable from
        // per_op_kernel via the standard scope-chain mechanism;
        // per-fiber state (cycle, scope values) propagates
        // correctly without external refresh.
        //
        // Borrow split: `self.main_kernel` is borrowed mut
        // through the closure; we capture an immutable borrow
        // of `self.main_kernel` separately and iterate
        // dispensers in a way that doesn't conflict.
        let dispenser_programs: Vec<Option<std::sync::Arc<polydat::kernel::PolydatProgram>>> =
            dispensers
                .iter()
                .map(|d| d.canonical_kernel().map(|k| k.program().clone()))
                .collect();
        // Build both the per-op kernels AND the parallel index
        // cache in one walk. The per-op index cache feeds
        // `reset_captures` so the per-cycle stanza-boundary
        // restore is pure indexed `set_input` — no per-call
        // `find_input` linear scan, no name hashing.
        let mut per_op_kernels: Vec<Option<PolydatKernel>> =
            Vec::with_capacity(dispenser_programs.len());
        let mut per_op_idx: Vec<Option<Vec<Option<usize>>>> =
            Vec::with_capacity(dispenser_programs.len());
        let mut per_op_side_effecting: Vec<Vec<String>> =
            Vec::with_capacity(dispenser_programs.len());
        for maybe_program in dispenser_programs {
            match maybe_program {
                None => {
                    per_op_kernels.push(None);
                    per_op_idx.push(None);
                    per_op_side_effecting.push(Vec::new());
                }
                Some(program) => {
                    let mut op_kernel = self
                        .main_kernel
                        .build_subscope(
                            polydat::kernel::subcontext::PolydatMatter::builder()
                                .program(program)
                                .build()
                                .expect("program-form matter is infallible"),
                        )
                        .expect("program-form subscope from fiber.main_kernel is infallible");
                    // Pre-resolve every scope value's input index
                    // against this op-template kernel's program.
                    let idx_vec: Vec<Option<usize>> = scope_values
                        .iter()
                        .map(|(name, _)| scope_value_index(op_kernel.program(), name))
                        .collect();
                    for ((name, value), idx_opt) in scope_values.iter().zip(idx_vec.iter()) {
                        if let Some(idx) = idx_opt {
                            crate::wires::write_input(&mut op_kernel, *idx, name, value.clone())
                                .unwrap_or_else(|e| {
                                    panic!(
                                        "scope value '{name}' failed typed write at op-template init: {e}"
                                    )
                                });
                        }
                    }
                    let const_outputs: Vec<String> = op_kernel
                        .program()
                        .const_outputs()
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    for init_name in &const_outputs {
                        // Const warmup is best-effort: a const
                        // whose freeze fails here stays dirty
                        // and re-evals (or fails visibly) at
                        // its first per-cycle use. But the
                        // failure is never discarded silently —
                        // the enriched payload goes to the
                        // session log so a broken const is
                        // diagnosable before the per-cycle
                        // path trips over it.
                        if let Err(payload) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                op_kernel.pull_ref(init_name);
                            }))
                        {
                            let msg = payload
                                .downcast_ref::<&'static str>()
                                .map(|s| (*s).to_string())
                                .or_else(|| payload.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "<non-string panic payload>".into());
                            crate::diag!(
                                crate::observer::LogLevel::Warn,
                                "const warmup pull '{init_name}' panicked during \
                                 op-template kernel init (deferring to first \
                                 per-cycle use): {msg}"
                            );
                        }
                    }
                    let side_effecting = op_kernel.program().outputs_with_side_effects();
                    per_op_kernels.push(Some(op_kernel));
                    per_op_idx.push(Some(idx_vec));
                    per_op_side_effecting.push(side_effecting);
                }
            }
        }
        self.per_op_kernels = per_op_kernels;
        self.per_op_side_effecting = per_op_side_effecting;
        self.scope_value_per_op_idx = per_op_idx;
    }

    /// Get the per-fiber kernel for the firing dispenser at
    /// `template_idx`. Returns `None` when the dispenser exposes
    /// no canonical kernel (adapters with no Polydat needs); callers
    /// fall back to the `NullWireSource` baseline.
    pub fn per_op_kernel(&self, template_idx: usize) -> Option<&PolydatKernel> {
        self.per_op_kernels
            .get(template_idx)
            .and_then(|s| s.as_ref())
    }

    /// Mutable accessor for the per-fiber kernel slot — used by
    /// cycle dispatch to wrap the kernel in [`crate::wires::CycleWires`]
    /// for `&mut`-requiring output pulls. Returns `None` when no
    /// canonical kernel was attached for this slot.
    pub fn per_op_kernel_mut(&mut self, template_idx: usize) -> Option<&mut PolydatKernel> {
        self.per_op_kernels
            .get_mut(template_idx)
            .and_then(|s| s.as_mut())
    }

    /// Mutable accessor for this fiber's main kernel — used by
    /// cycle dispatch when the firing dispenser exposes no
    /// canonical op-template kernel (the flattened path). The
    /// per-op kernel path uses [`Self::per_op_kernel_mut`]
    /// instead.
    pub fn main_kernel_mut(&mut self) -> &mut PolydatKernel {
        &mut self.main_kernel
    }

    /// Borrow this fiber's main Polydat program.
    pub fn program(&self) -> &Arc<PolydatProgram> {
        self.main_kernel.program()
    }

    /// Mutable access to the fiber's main `PolydatState`. The state
    /// lives inside `main_kernel`; this accessor preserves the
    /// pre-restructure call shape for sites that wrote
    /// `fiber.state.…` directly.
    pub fn state(&mut self) -> &mut PolydatState {
        self.main_kernel.state()
    }

    /// Borrowed (`&`) accessor for the main state.
    pub fn state_ref(&self) -> &PolydatState {
        self.main_kernel.state_ref()
    }

    /// Set coordinates and begin a new evaluation scope.
    ///
    /// Bounded by [`PolydatProgram::coord_count`]: the slice is
    /// truncated to the program's declared coordinate count
    /// before being written. A phase kernel with no
    /// coordinates (e.g. all bindings are invariant within the
    /// stanza, only externs declared) has `coord_count = 0`,
    /// so this becomes a no-op rather than clobbering the
    /// extern slots that follow.
    ///
    /// SRD-13d Phase 9: the same coordinates are also written to
    /// every per-op-template kernel that declares them as
    /// coords. Each kernel binds its own input slot for `cycle`
    /// (cascaded from parent) so per-cycle propagation is a
    /// per-kernel `set_inputs`, not a chain walk.
    pub fn set_inputs(&mut self, coords: &[u64]) {
        let main_n = coords.len().min(self.main_kernel.program().coord_count());
        if main_n > 0 {
            self.main_kernel.state().set_inputs(&coords[..main_n]);
        }
        // SRD-68 per-fiber kernels: same per-cycle propagation.
        // Each per-op kernel declares its own coord_count (the
        // ones it auto-externs from the parent's coord names —
        // typically just `cycle`). Skip slots without a kernel
        // (adapter exposed no canonical_kernel).
        for kernel in self.per_op_kernels.iter_mut().flatten() {
            let n = coords.len().min(kernel.program().coord_count());
            if n > 0 {
                kernel.state().set_inputs(&coords[..n]);
            }
        }
    }

    /// Feed a source item into the Polydat state.
    ///
    /// Sets the ordinal as the coordinate input and injects field
    /// projections into the appropriate state slots (e.g.
    /// `base__ordinal`, `base__vector`).
    ///
    /// The ordinal write is bounded by [`PolydatProgram::coord_count`]
    /// — phases whose programs declare no coordinates (only
    /// externs and stanza-invariant bindings) skip the write
    /// rather than clobbering an extern slot. Field
    /// projections always write by name, so they're safe
    /// regardless of coord_count.
    ///
    /// SRD-13d Phase 9: ordinal + fields propagate to every
    /// op-template kernel that declares matching slots.
    pub fn set_source_item(&mut self, item: &polydat::iteration::source::SourceItem) {
        if self.main_kernel.program().coord_count() > 0 {
            self.main_kernel.state().set_inputs(&[item.ordinal]);
        }
        // Source items carry typed values from their upstream
        // DataSource; one the slot's type cannot take, converted or
        // not, means the source produced an incompatible value, which
        // is a fail-loud condition.
        for (name, value) in &item.fields {
            if let Some(idx) = self.main_kernel.program().find_input(name) {
                crate::wires::write_input(&mut self.main_kernel, idx, name, value.clone())
                    .unwrap_or_else(|e| {
                        panic!("source item field '{name}' failed typed write: {e}")
                    });
            }
        }
        // Cell-bound cross-fiber visibility is now substrate-
        // owned via the per-cell revision counter + per-scope
        // intent-dirty vector + per-fiber `last_seen` (see
        // polydat/docs/design/cross_fiber_invalidation.md):
        // ancestor writes through a `SharedCell` bump the
        // cell's revision and set its intent bit, and our
        // `eval_node` walker re-checks both before serving a
        // memoized result. No host-side refresh call needed.
        // SRD-68 per-fiber kernels: same per-cycle propagation.
        for kernel in self.per_op_kernels.iter_mut().flatten() {
            if kernel.program().coord_count() > 0 {
                kernel.state().set_inputs(&[item.ordinal]);
            }
            for (name, value) in &item.fields {
                if let Some(idx) = kernel.program().find_input(name) {
                    kernel.state().set_input(idx, value.clone());
                }
            }
        }
        // SRD-13f Push D: only advance_broadcasts when a
        // descendant per-op kernel has a *different* program
        // from main_kernel. When the per-op kernel reuses
        // main_kernel's program (the flattened-op-template
        // path — no per-op matter), evaluating outputs here is
        // both redundant (the descendant evaluates the same
        // wires locally) and harmful (side-effecting nodes
        // like `testkit_throw_at` fire outside the per-op cascade
        // surface, losing the panic-to-error pipeline).
        // When the per-op kernel has its own program, it
        // carries `extern <name>` slots for cross-scope wires
        // it doesn't replicate locally; those slots need
        // main_kernel to compute and broadcast the values
        // through the cell, so the descendant's read picks up
        // the current value via the cell-aware input path.
        let main_program_ptr = std::sync::Arc::as_ptr(self.main_kernel.program());
        let needs_broadcast = self.per_op_kernels.iter().any(|slot| {
            slot.as_ref()
                .is_some_and(|k| std::sync::Arc::as_ptr(k.program()) != main_program_ptr)
        });
        if needs_broadcast {
            self.main_kernel.advance_broadcasts();
        }
    }

    /// Reset capture inputs to defaults. Called at stanza
    /// boundaries to prevent capture leakage across stanzas.
    /// Coordinates are not reset. Scope-bound iter-var inputs
    /// (set by [`OpBuilder::create_fiber_builder`]) are
    /// re-applied after the reset so the iteration's bound
    /// values survive the boundary.
    pub fn reset_captures(&mut self) {
        let coord_count = self.main_kernel.program().coord_count();
        self.main_kernel.state().reset_inputs_from(coord_count);
        // Re-apply scope values via cached indices — the per-cycle
        // hot path was previously dominated by `find_input`
        // linear scans here. Indices were resolved once at
        // `set_scope_values` time.
        for ((_name, value), idx_opt) in self
            .scope_values
            .iter()
            .zip(self.scope_value_main_idx.iter())
        {
            if let Some(idx) = idx_opt {
                self.main_kernel.state().set_input(*idx, value.clone());
            }
        }
        // SRD-68 per-fiber kernels: each one's coord count comes
        // from its own program. Re-apply scope values via the
        // pre-resolved per-op index cache populated at
        // `attach_dispenser_kernels` time.
        for (slot, idx_slot) in self
            .per_op_kernels
            .iter_mut()
            .zip(self.scope_value_per_op_idx.iter())
        {
            if let (Some(kernel), Some(idx_vec)) = (slot, idx_slot) {
                let n = kernel.program().coord_count();
                kernel.state().reset_inputs_from(n);
                for ((_name, value), idx_opt) in self.scope_values.iter().zip(idx_vec.iter()) {
                    if let Some(idx) = idx_opt {
                        kernel.state().set_input(*idx, value.clone());
                    }
                }
            }
        }
    }

    /// Invalidate all state: reset all inputs and mark all nodes dirty.
    /// Provides "clean slate" semantics.
    pub fn invalidate_all(&mut self) {
        self.main_kernel.state().invalidate_all();
        for kernel in self.per_op_kernels.iter_mut().flatten() {
            kernel.state().invalidate_all();
        }
    }

    /// Store a captured value directly into Polydat state.
    ///
    /// Writes to the named port in PolydatState. Returns `true` if the
    /// port was found and the value stored, `false` if no port with
    /// this name exists in the program (value is dropped).
    pub fn capture(&mut self, name: &str, value: Value) -> bool {
        if let Some(idx) = self.main_kernel.program().find_input(name) {
            self.main_kernel.state().set_input(idx, value);
            true
        } else {
            false
        }
    }

    /// SRD-67 Phase 5 — write a value into a specific op-template
    /// kernel's input slot by name. No-op when (a) the op didn't
    /// materialise a kernel (flattened op-template), or (b) the
    /// kernel doesn't declare an input slot for `name` (the
    /// closure-binding economy dropped it because the source
    /// doesn't reference it).
    ///
    /// Used by the activity loop's post-`execute` step to feed
    /// SRD-66 result-binding inputs (`body` / `count` / `ok` and
    /// any captures) into the op-template kernel before
    /// [`Self::commit_op_template_write_throughs`] fans the
    /// computed values up through parent `shared` cells.
    /// SRD-68 Push 5d: position-indexed write into the per-fiber
    /// op-template kernel slot. Used by the cycle dispatch's
    /// post-execute capture flow to feed result-binding inputs
    /// (`body` / `count` / `ok` and any captures) into the kernel
    /// before [`Self::commit_op_template_write_throughs_for_idx`]
    /// fans the computed values up through parent `shared` cells.
    pub fn write_op_template_input_for_idx(
        &mut self,
        template_idx: usize,
        name: &str,
        value: Value,
    ) -> bool {
        let Some(kernel) = self
            .per_op_kernels
            .get_mut(template_idx)
            .and_then(|s| s.as_mut())
        else {
            if nmbrs_dirty_debug_enabled() && name == "body" {
                eprintln!("DIRTY: write body template={template_idx} NO_KERNEL");
            }
            return false;
        };
        let Some(idx) = kernel.program().find_input(name) else {
            if nmbrs_dirty_debug_enabled() && name == "body" {
                let inputs = kernel.program().input_names();
                eprintln!(
                    "DIRTY: write body template={template_idx} NO_SLOT in_count={} names={:?}",
                    inputs.len(),
                    inputs
                );
            }
            return false;
        };
        if nmbrs_dirty_debug_enabled() && name == "body" {
            let input_count = kernel.program().input_names().len();
            let display = value.to_display_string();
            let head: String = display.chars().take(48).collect();
            eprintln!(
                "DIRTY: write body template={template_idx} idx={idx} in_count={input_count} \
                 head=\"{head}\""
            );
        }
        kernel.state().set_input(idx, value);
        true
    }

    /// SRD-67 Phase 5 — invoke the op-template kernel's Rule 2
    /// write-through commit, propagating each result-binding LHS
    /// value to the parent's `SharedCell` (and from there to any
    /// sibling phase that imports the same name). No-op when the
    /// op's kernel carries no write-throughs (the typical case
    /// for ops without `result:`).
    /// SRD-68 Push 5d: position-indexed Rule 2 write-through commit.
    /// Pulls every `__write_<X>` and stores its value through the
    /// cell-bound input slot for `<X>`, propagating each result-
    /// binding LHS value to the parent's `SharedCell` (and from
    /// there to any sibling phase that imports the same name).
    /// No-op when the kernel carries no write-throughs (typical
    /// for ops without `result:`).
    /// Errors when a write-through violates cell type stability
    /// (scope_model.md §"Type stability") — surfaced by the fiber
    /// loop as a phase-stopping workload bug (it is deterministic:
    /// every subsequent cycle would repeat it).
    pub fn commit_op_template_write_throughs_for_idx(
        &mut self,
        template_idx: usize,
    ) -> Result<(), String> {
        let debug = polydat::library::debug_nodes_enabled();
        let Some(kernel) = self
            .per_op_kernels
            .get_mut(template_idx)
            .and_then(|s| s.as_mut())
        else {
            if debug {
                crate::observer::log(
                    crate::observer::LogLevel::Debug,
                    &format!(
                        "commit_op_template_write_throughs_for_idx: template_idx {template_idx} \
                         has no per-fiber kernel slot"
                    ),
                );
            }
            return Ok(());
        };
        if debug {
            crate::observer::log(
                crate::observer::LogLevel::Debug,
                &format!(
                    "commit_op_template_write_throughs_for_idx: idx {template_idx} kernel found"
                ),
            );
        }
        kernel.commit_write_throughs()
    }

    /// Per-cycle: pull the op-template kernel's **side-effecting**
    /// outputs at `template_idx` so side-effecting nodes (`log_info`
    /// and friends) actually evaluate. Without this, captured wires
    /// whose only consumer is a write-through are pulled by
    /// `commit_write_throughs`, but captured wires that aren't shared
    /// with a parent never get pulled — their compute chain (including
    /// any side-effecting nodes inside it) stays dormant, and the
    /// diagnostic the workload asked for never fires.
    ///
    /// Only outputs whose cone contains a `Purity::SideChannel` node are
    /// pulled (the set is precomputed at attach time,
    /// [`PolydatProgram::outputs_with_side_effects`]). A side-effect-free
    /// output is **not** pulled here: re-evaluating it would do nothing,
    /// and for a volatile metric reader (a `metricsql_*` / `metric`
    /// objective binding) it would issue a live metrics query *every
    /// cycle*. Such values are evaluated only when actually consumed.
    /// No-op when the kernel has no side-effecting outputs.
    pub fn pull_all_op_template_outputs_for_idx(&mut self, template_idx: usize) {
        let Some(names) = self.per_op_side_effecting.get(template_idx) else {
            return;
        };
        if names.is_empty() {
            return;
        }
        let names = names.clone();
        let Some(kernel) = self
            .per_op_kernels
            .get_mut(template_idx)
            .and_then(|s| s.as_mut())
        else {
            return;
        };
        for name in &names {
            let _ = kernel.pull_ref(name);
        }
    }

    /// Materialize a [`PullPlan`] against this fiber's main PolydatState.
    /// O(plan_len) on the hot path, no name hashing — the plan
    /// holds pre-resolved indices.
    ///
    /// This is the cycle-time read path used by every wrapper that
    /// holds [`PullHandle`]s registered into the corresponding
    /// [`ScopeFixture`] at init (SRD 31 §"Pull plan vs bind plan",
    /// SRD 32 §"Init-Time Fixture and Consumer Self-Registration").
    ///
    /// [`PullPlan`]: crate::fixture::PullPlan
    /// [`PullHandle`]: crate::fixture::PullHandle
    /// [`ScopeFixture`]: crate::fixture::ScopeFixture
    pub fn resolve_pulls(
        &mut self,
        plan: &crate::fixture::PullPlan,
    ) -> crate::fixture::ResolvedPulls {
        plan.resolve(self.main_kernel.state())
    }

    /// SRD-68 Push 5d resolve path — picks the right `PolydatState`
    /// for the dispenser at `template_idx` and resolves the
    /// plan against it. When a per-fiber op-template kernel was
    /// instanced for that position (every adapter exposes
    /// `canonical_kernel()` so this is the typical case), its
    /// state is used; otherwise the plan resolves against the
    /// fiber's main kernel state (the flattened op-template path).
    pub fn resolve_pulls_for_idx(
        &mut self,
        template_idx: usize,
        plan: &crate::fixture::PullPlan,
    ) -> crate::fixture::ResolvedPulls {
        match self
            .per_op_kernels
            .get_mut(template_idx)
            .and_then(|s| s.as_mut())
        {
            Some(kernel) => {
                plan.check_program_match(kernel.program(), template_idx);
                plan.resolve(kernel.state())
            }
            None => {
                plan.check_program_match(self.main_kernel.program(), template_idx);
                plan.resolve(self.main_kernel.state())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polydat::compile::assembly::{PolydatAssembler, WireRef};
    use polydat::library::arithmetic::Mod;
    use polydat::library::hash::Hash;

    fn make_kernel() -> PolydatKernel {
        let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
        asm.add_node(
            "hashed",
            Box::new(Hash::new()),
            vec![WireRef::input("cycle")],
        );
        asm.add_node(
            "user_id",
            Box::new(Mod::new(1_000_000)),
            vec![WireRef::node("hashed")],
        );
        asm.add_output("user_id", WireRef::node("user_id"));
        asm.add_output("hashed", WireRef::node("hashed"));
        asm.compile().unwrap()
    }

    /// SRD-13f Stage 1 gate — verify that the per-fiber
    /// kernel chain is a single linear descent. The
    /// op-template (per-op) kernel must be built as a
    /// subscope of the fiber's main kernel, NOT of a
    /// separate shared canonical. Inner reads of cross-scope
    /// wires depend on this being a per-fiber-consistent
    /// chain so cell-attached values reach inner via outer's
    /// per-fiber pull writes.
    #[test]
    fn per_fiber_chain_is_linear_and_unshared() {
        use crate::adapter::OpDispenser;
        // The structural property under test: when a fiber
        // attaches per-op kernels from a dispenser-shaped
        // canonical, each per-op kernel must be a NEW
        // per-fiber instance (subscope of the fiber's main
        // kernel), not the shared canonical kernel itself.
        //
        // Two fibers attaching against the same canonical
        // must produce different per-op kernel instances.
        let workload_kernel = make_kernel();
        let builder = OpBuilder::new(workload_kernel);

        // Stand up a shared canonical via the public API.
        let workload_src = "input cycle: u64\nfolded := 42\n";
        let canonical_program = polydat::dsl::compile::compile_polydat_interpreter(workload_src)
            .expect("compile probe canonical")
            .program()
            .clone();
        let canonical_kernel: std::sync::Arc<PolydatKernel> =
            builder.canonical_kernel_for_op("nonexistent");
        // For this probe we only need the canonical to expose
        // a program; reuse builder's source_kernel program.
        let _ = canonical_program;

        struct ProbeDispenser(std::sync::Arc<PolydatKernel>);
        impl OpDispenser for ProbeDispenser {
            fn canonical_kernel(&self) -> Option<&std::sync::Arc<PolydatKernel>> {
                Some(&self.0)
            }
            fn execute<'a>(
                &'a self,
                _cycle: u64,
                _ctx: &'a crate::fixture::ExecCtx<'a>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                crate::adapter::OpResult,
                                crate::adapter::ExecutionError,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                Box::pin(async move { Ok(crate::adapter::OpResult::default()) })
            }
        }
        let dispensers: Vec<std::sync::Arc<dyn OpDispenser>> = vec![std::sync::Arc::new(
            ProbeDispenser(canonical_kernel.clone()),
        )];

        let mut fiber_a = builder.create_fiber_builder();
        fiber_a.attach_dispenser_kernels(&dispensers);
        let mut fiber_b = builder.create_fiber_builder();
        fiber_b.attach_dispenser_kernels(&dispensers);

        let per_op_a = fiber_a.per_op_kernel(0).expect("per-op A attached");
        let per_op_b = fiber_b.per_op_kernel(0).expect("per-op B attached");

        // SRD-13f invariant: each fiber's per-op kernel is a
        // distinct per-fiber instance, neither of them
        // pointing to the shared canonical kernel.
        assert!(
            !std::ptr::eq(
                per_op_a as *const PolydatKernel,
                canonical_kernel.as_ref() as *const PolydatKernel
            ),
            "per_op_a must be a distinct per-fiber instance, \
             not the shared canonical",
        );
        assert!(
            !std::ptr::eq(
                per_op_b as *const PolydatKernel,
                canonical_kernel.as_ref() as *const PolydatKernel
            ),
            "per_op_b must be a distinct per-fiber instance, \
             not the shared canonical",
        );
        assert!(
            !std::ptr::eq(
                per_op_a as *const PolydatKernel,
                per_op_b as *const PolydatKernel
            ),
            "fiber A and fiber B must each have their own \
             per-op kernel instance",
        );
    }

    /// SRD 11 §"Init Binding Contract" Plan B verification:
    /// after the activation kernel pulls an init binding, the
    /// pulled value must propagate to every fiber via
    /// `init_overrides`, and per-fiber pulls must read the seeded
    /// buffer rather than re-firing the eval.
    #[test]
    fn init_binding_fires_once_across_many_fibers() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicU64, Ordering};

        // Counting custom node: bumps a shared counter on every
        // eval call, returns U64(42). Tracks how many times its
        // eval body actually runs across the test.
        struct CountingNode {
            meta: polydat::ast::NodeMeta,
            calls: StdArc<AtomicU64>,
        }
        impl polydat::ast::PolydatNode for CountingNode {
            fn meta(&self) -> &polydat::ast::NodeMeta {
                &self.meta
            }
            fn eval(&self, _inputs: &[Value], outputs: &mut [Value]) {
                self.calls.fetch_add(1, Ordering::Relaxed);
                outputs[0] = Value::U64(42);
            }
        }

        let calls = StdArc::new(AtomicU64::new(0));
        let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
        // Compile-const seed expression — wires empty.
        asm.add_node(
            "ticks",
            Box::new(CountingNode {
                meta: polydat::ast::NodeMeta {
                    name: "ticks".into(),
                    outs: vec![polydat::ast::Port::new(
                        "output",
                        polydat::ast::PortType::U64,
                    )],
                    ins: vec![],
                },
                calls: calls.clone(),
            }),
            vec![],
        );
        asm.add_output("ticks", WireRef::node("ticks"));
        asm.mark_const_output("ticks");

        let mut kernel = asm.compile().expect("compile");
        // Plan B normally runs in the executor; for this unit test
        // we simulate it by pulling the init binding once on the
        // activation kernel.
        let v = kernel.pull_ref("ticks").clone();
        assert_eq!(v, Value::U64(42));
        let after_pull = calls.load(Ordering::Relaxed);
        // The fold pass evaluates the node once, then ConstU64
        // replaces it (init binding is pure compile-const here),
        // so subsequent state pulls return the leaf const without
        // re-eval. Assert the call count never grows from here.
        let builder = OpBuilder::new(kernel);

        // Spawn many fibers, each pulls the init binding. None
        // should trigger an eval — the init_overrides path seeds
        // the buffer, and the post-fold leaf-const path returns
        // the constant directly.
        for _ in 0..32 {
            let mut fiber = builder.create_fiber_builder();
            fiber.set_inputs(&[0]);
            let pulled = fiber.state().pull(&builder.program(), "ticks").clone();
            assert_eq!(pulled, Value::U64(42));
        }
        let after_fibers = calls.load(Ordering::Relaxed);
        assert_eq!(
            after_pull, after_fibers,
            "init binding 'ticks' eval must not re-fire across fibers \
             (eval calls before fibers: {after_pull}, after 32 fibers: {after_fibers})"
        );
        // Independent: confirm the eval ran at most once during
        // compile-time fold + the activation pull.
        assert!(
            after_fibers <= 1,
            "expected at most one eval across compile fold + activation pull, got {after_fibers}"
        );
    }
}
