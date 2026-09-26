// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-fiber Polydat Kernel construction.
//!
//! `OpBuilder` owns the activity's source kernel and seeds each
//! per-fiber [`FiberBuilder`] with a kernel bound under it, on the
//! fiber engine where it has an image ([`crate::fiber_engine`]), plus
//! the named scope overrides. The
//! adapter-facing cycle-time bind-point resolution path
//! historically lived here too, but SRD-68 Push 5 retired it in
//! favour of the generic [`crate::wires::WireSource`] surface;
//! this module now scopes to fiber construction and bind-point
//! validation.
//!
//! See `docs/SRD/68_dispenser_owned_polydat_context.md` for the
//! resolution model.

use std::sync::{Arc, OnceLock};

use crate::fiber_engine::OpTemplateModule;
use nmbrs_workload::bindpoints::{self, BindPoint, BindQualifier};
use nmbrs_workload::model::ParsedOp;
use polydat::Kernel;
use polydat::ast::Value;
use polydat::kernel::{KernelProgram, PolydatKernel, PolydatProgram};

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
/// Holds the activity's source kernel (immutable, shared). Each
/// executor fiber calls `create_fiber_builder()` to get its own
/// `FiberBuilder` with private kernels — no locks, no contention on
/// the hot path.
pub struct OpBuilder {
    /// Name-keyed scope values (per SRD-13c), written into every new
    /// fiber's kernels. Stored by name rather than `(input_idx, value)`
    /// because each kernel — the fiber main kernel, every per-op-template
    /// kernel — owns its own input layout, and an index captured against
    /// the source kernel doesn't translate. The previous
    /// `Vec<(usize, Value)>` shape silently mis-routed writes across
    /// kernels (e.g. `table` value landing in the `profile` slot of an
    /// op-template kernel whose extern declaration order differed from
    /// the phase scope).
    scope_values: Vec<(String, Value)>,
    /// The source kernel — the activity's own kernel that each
    /// per-fiber `FiberBuilder` binds its main kernel under. Owning the
    /// kernel (not just its program) carries the activity's full cell
    /// state — own input-slot cells plus transit cells inherited from
    /// ancestors — to every fiber's main kernel.
    source_kernel: Arc<PolydatKernel>,
    /// SRD-13d Phase 9 — per-op-template kernel programs keyed
    /// by op name. Populated by [`Self::with_op_template_programs`]
    /// when the runner has materialised op-template kernels in
    /// the scope tree. Wrappers (e.g. `MetricsDispenser`) look up
    /// the program for their template via [`Self::program_for_op`]
    /// and build their `ScopeFixture` against it; flattened
    /// op-templates fall through to the activity-wide `program`.
    op_template_programs: std::collections::HashMap<String, Arc<PolydatProgram>>,
    /// The fiber engine's image of the source kernel's program, which
    /// every fiber's main kernel runs; `None` keeps fibers on the
    /// interpreter.
    fiber_image: Option<Arc<dyn KernelProgram>>,
    /// What each canonical kernel this builder hands out stands for,
    /// keyed by its `program_id`: a dispenser's canonical may be on any
    /// engine, and each fiber finds here the interpreter program that
    /// resolves its indices and the module it instantiates the per-op
    /// kernel from.
    canonicals: Canonicals,
}

/// What a canonical kernel's program is to a fiber: the interpreter
/// program that resolves its indices (the analysis program; a native
/// image of it shares its indices, `fiber_engine::agrees`), and the
/// op-template module a per-op kernel is instantiated from on the fiber
/// engine, when it has one.
#[derive(Clone)]
struct CanonicalSource {
    program: Arc<PolydatProgram>,
    module: Option<Arc<OpTemplateModule>>,
}

/// Canonical sources by the `program_id` of every kernel that stands for
/// them.
type Canonicals = Arc<std::collections::HashMap<polydat::kernel::ProgramId, CanonicalSource>>;

/// The identity an interpreter program's kernels report.
fn program_id_of(program: &Arc<PolydatProgram>) -> polydat::kernel::ProgramId {
    KernelProgram::program_id(program.as_ref())
}

impl OpBuilder {
    /// Create an OpBuilder from a kernel.
    ///
    /// If the kernel has scope values (set via `materialize_wiring_from_outer`
    /// or directly via `kernel.state().set_input`), they are
    /// captured and propagated into every fiber's kernels.
    pub fn new(kernel: impl Into<Arc<PolydatKernel>>) -> Self {
        let kernel: Arc<PolydatKernel> = kernel.into();
        // Scope values seed PLAIN slots only. A CELL-BOUND slot's value
        // is the live shared cell — snapshotting it here freezes the
        // cell's activity-start value, and every downstream
        // re-application (fiber seeding, the stanza-boundary
        // `reset_captures`) would `set_input` that stale snapshot
        // THROUGH the cell, clobbering later writes for every kernel
        // sharing it. Same exclusion `reset_inputs` documents:
        // cells are cross-kernel shared state with their own
        // lifecycle.
        let scope_values: Vec<(String, Value)> = kernel
            .scope_values()
            .into_iter()
            .filter(|(name, _)| {
                kernel
                    .program()
                    .find_input(name)
                    .map(|idx| !Kernel::input_is_cell_bound(kernel.as_ref(), idx))
                    .unwrap_or(true)
            })
            .collect();
        // The source kernel is the canonical of every flattened op.
        let canonicals = std::iter::once((
            kernel.program_id(),
            CanonicalSource {
                program: kernel.program().clone(),
                module: None,
            },
        ))
        .collect();
        Self {
            scope_values,
            source_kernel: kernel,
            op_template_programs: std::collections::HashMap::new(),
            fiber_image: None,
            canonicals: Arc::new(canonicals),
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
        let canonicals = Arc::make_mut(&mut self.canonicals);
        for program in programs.values() {
            canonicals
                .entry(program_id_of(program))
                .or_insert_with(|| CanonicalSource {
                    program: program.clone(),
                    module: None,
                });
        }
        self.op_template_programs = programs;
        self
    }

    /// Run every fiber's main kernel on `image`, the fiber engine's
    /// image of the source kernel's program ([`crate::fiber_engine`]).
    pub fn with_fiber_image(mut self, image: Option<Arc<dyn KernelProgram>>) -> Self {
        self.fiber_image = image;
        self
    }

    /// Instantiate per-op kernels from `modules`, the op-template scope
    /// modules of this phase. A module whose fiber-engine image
    /// disagrees with its program is left out, and its per-op kernels
    /// stay on the interpreter.
    pub fn with_op_template_modules(
        mut self,
        modules: impl IntoIterator<Item = (String, Arc<OpTemplateModule>)>,
    ) -> Self {
        let canonicals = Arc::make_mut(&mut self.canonicals);
        for (name, module) in modules {
            let Some(image) =
                crate::fiber_engine::module_image(&module, &format!("op template '{name}'"))
            else {
                continue;
            };
            let source = CanonicalSource {
                program: module.program().clone(),
                module: Some(module.clone()),
            };
            // Both the interpreter program and its native image stand
            // for the module: a canonical kernel reports either.
            canonicals.insert(program_id_of(module.program()), source.clone());
            canonicals.insert(image.program_id(), source);
        }
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
    /// I-4). Built once at dispenser construction time.
    ///
    /// When `op_name` has a registered op-template program (phase
    /// `bindings:`, op-level `bindings:`, `result:` block — the
    /// matter assembled by the synthesis pipeline before the
    /// activity runs), the canonical is that program bound under
    /// `source_kernel`: instantiated from its module on the fiber
    /// engine when it has one, on the interpreter otherwise.
    /// Otherwise the canonical is the source kernel itself
    /// (Arc-cloned), which covers the flattened-op-template path (no
    /// per-op matter). Either way the adapter holds a kernel of any
    /// engine, whose `program_id` this builder recognizes.
    pub fn canonical_kernel_for_op(&self, op_name: &str) -> Arc<dyn Kernel> {
        let Some(program) = self.op_template_programs.get(op_name) else {
            return self.source_kernel.clone();
        };
        let module = self
            .canonicals
            .get(&program_id_of(program))
            .and_then(|source| source.module.clone());
        match module {
            Some(module) => Arc::from(
                module
                    .instantiate_under(
                        self.source_kernel.as_ref(),
                        crate::fiber_engine::fiber_engine(),
                        &[],
                    )
                    .unwrap_or_else(|e| {
                        panic!("op '{op_name}': canonical kernel failed to instantiate: {e}")
                    }),
            ),
            None => Arc::from(
                polydat::kernel::bind_under(
                    self.source_kernel.as_ref(),
                    program.clone() as Arc<dyn KernelProgram>,
                    &[],
                )
                .unwrap_or_else(|e| panic!("op '{op_name}': canonical kernel failed to bind: {e}")),
            ),
        }
    }

    /// Create a per-fiber builder. No locks, no sharing — the fiber
    /// owns its kernels exclusively. Scope values (per-iteration
    /// inputs from `for_each` / `for_combinations` / outer scope
    /// constants) are written into the main kernel's inputs and
    /// remembered on the builder so `reset_captures` (called at
    /// stanza boundaries) can re-apply them — otherwise the
    /// blanket "reset all non-coord inputs" pass would clobber
    /// the iteration's bound values.
    pub fn create_fiber_builder(&self) -> FiberBuilder {
        // The fiber's main kernel is bound under the activity's source
        // kernel: cells, transit cells, and value-copy bindings flow in
        // automatically, and the scope-init constants materialize, so
        // the fiber observes the same cell handles as the workload-root
        // through the chain.
        //
        // Scope values are bound with it, as iteration bindings, so the
        // main kernel's consts initialize from them (a const is fixed at
        // init). The indices cached here feed the per-cycle
        // `reset_captures` re-application.
        let mut fb = FiberBuilder::with_scope(
            &self.source_kernel,
            self.fiber_image.clone(),
            self.scope_values.clone(),
        );
        fb.canonicals = self.canonicals.clone();
        // SRD-68: per-fiber op-template kernels are populated by
        // `attach_dispenser_kernels`, which runs right after this
        // function returns (see executor cycle dispatch), each from
        // the firing dispenser's `OpDispenser::canonical_kernel()`.
        fb
    }
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

/// The scope values a kernel of `program` takes, as the `iter_bindings`
/// its binder writes before the kernel's consts are initialized: each
/// value converted to its slot's declared type, skipping the values
/// `program` has no (non-coordinate) input for.
///
/// # Panics
/// On a scope value its slot's type cannot take, converted or not: a
/// fail-loud condition, since it would otherwise corrupt every read.
fn scope_bindings(
    program: &PolydatProgram,
    scope_values: &[(String, Value)],
) -> Vec<(String, Value)> {
    scope_values
        .iter()
        .filter(|(name, _)| scope_value_index(program, name).is_some())
        .map(|(name, value)| {
            let value = match program.input_port_type(name) {
                Some(port) => polydat::convert::to_port(value.clone(), port).unwrap_or_else(|e| {
                    panic!("scope value '{name}' failed typed write at scope-init: {e}")
                }),
                None => value.clone(),
            };
            (name.clone(), value)
        })
        .collect()
}

/// Per-fiber op builder. Owns its own kernels.
/// No locks, no synchronization, no contention.
///
/// Created via `OpBuilder::create_fiber_builder()` at fiber startup.
///
/// Every kernel here may be on any engine: the main kernel and the
/// per-op kernels run on the fiber engine where an image is available
/// ([`crate::fiber_engine`]) and on the interpreter otherwise. Each is
/// paired with the interpreter program it runs or was imaged from,
/// which shares its input and output indices: names resolve on the
/// program once, and the kernel is driven by index.
pub struct FiberBuilder {
    /// The fiber's main kernel — typically the activity-wide
    /// (workload / phase) program.
    main_kernel: Box<dyn Kernel>,
    /// The interpreter program [`Self::main_kernel`] runs or was
    /// imaged from.
    main_program: Arc<PolydatProgram>,
    /// Scope-bound input values (per-iteration extern bindings)
    /// that should persist across stanza-level `reset_inputs`
    /// resets. Empty for a builder created via plain
    /// [`FiberBuilder::new`]; populated by
    /// [`OpBuilder::create_fiber_builder`].
    scope_values: Vec<(String, Value)>,
    /// SRD-68 invariant I-4 — per-fiber kernel instances, indexed
    /// parallel to the activity's dispenser registry. Each entry
    /// is the corresponding dispenser's canonical program bound under
    /// [`Self::main_kernel`]; `None` for dispensers that don't expose
    /// a canonical kernel (adapters with no Polydat needs, or wrappers
    /// that delegate). Populated by
    /// [`Self::attach_dispenser_kernels`] right after fiber spawn,
    /// before any cycles run; read at cycle dispatch to populate
    /// `ExecCtx::wires` for the firing dispenser.
    per_op_kernels: Vec<Option<Box<dyn Kernel>>>,
    /// The interpreter program each per-op kernel runs or was imaged
    /// from, parallel to [`Self::per_op_kernels`].
    per_op_programs: Vec<Option<Arc<PolydatProgram>>>,
    /// Per-op-kernel side-effecting output indices (parallels
    /// [`Self::per_op_kernels`]). The subset of each op-template
    /// kernel's outputs whose cone contains a `Purity::SideChannel`
    /// node — the only outputs the per-cycle "fire side effects" pass
    /// pulls. Computed once at [`Self::attach_dispenser_kernels`] so
    /// volatile metric-reader outputs (the objective bindings) are NOT
    /// re-evaluated every cycle just to fire a non-existent effect.
    per_op_side_effecting: Vec<Vec<usize>>,
    /// Pre-resolved input indices for each entry in
    /// [`Self::scope_values`] against [`Self::main_kernel`].
    /// `None` slots are scope values the main program doesn't
    /// declare, or declares as a coordinate (silently skipped at
    /// write time — matches the historical name-based-skip semantics).
    scope_value_main_idx: Vec<Option<usize>>,
    /// Per-op-kernel mirror of [`Self::scope_value_main_idx`].
    /// Outer Vec parallels [`Self::per_op_kernels`]; inner Vec
    /// parallels [`Self::scope_values`]. `None` outer slots
    /// match per_op_kernels' `None` entries.
    scope_value_per_op_idx: Vec<Option<Vec<Option<usize>>>>,
    /// Whether some per-op kernel runs a program other than the main
    /// kernel's, and so reads the main kernel's outputs through its
    /// broadcast cells (see [`Self::set_source_item`]).
    needs_broadcast: bool,
    /// What each dispenser's canonical kernel stands for — its
    /// interpreter program and op-template module — by the kernel's
    /// `program_id` (see [`OpBuilder::canonical_kernel_for_op`]).
    canonicals: Canonicals,
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
    /// Create a new fiber builder whose main kernel is the parent's own
    /// program bound under the parent, on the interpreter.
    pub fn new(parent: &PolydatKernel) -> Self {
        Self::with_image(parent, None)
    }

    /// Create a new fiber builder whose main kernel runs `image` — the
    /// fiber engine's image of `parent`'s program — bound under
    /// `parent`; the interpreter program itself when `image` is `None`.
    /// Per-fiber state is fresh; cell handles are Arc-shared with the
    /// parent so writes propagate to the workload-root through the
    /// cascade.
    pub fn with_image(parent: &PolydatKernel, image: Option<Arc<dyn KernelProgram>>) -> Self {
        Self::with_scope(parent, image, Vec::new())
    }

    /// [`Self::with_image`], binding `scope_values` into the main kernel
    /// as it is built, before its consts initialize, and remembering
    /// them for the stanza-boundary [`Self::reset_captures`].
    pub fn with_scope(
        parent: &PolydatKernel,
        image: Option<Arc<dyn KernelProgram>>,
        scope_values: Vec<(String, Value)>,
    ) -> Self {
        let main_program = parent.program().clone();
        let image: Arc<dyn KernelProgram> = image.unwrap_or_else(|| main_program.clone());
        let bindings = scope_bindings(&main_program, &scope_values);
        let main_kernel = polydat::kernel::bind_under(parent, image, &bindings)
            .unwrap_or_else(|e| panic!("a fiber's main kernel failed to bind: {e}"));
        let scope_value_main_idx = scope_values
            .iter()
            .map(|(name, _)| scope_value_index(&main_program, name))
            .collect();
        // Standing alone, the fiber knows one canonical: its parent's own
        // program, the flattened op's. `OpBuilder::create_fiber_builder`
        // replaces this with the activity's full set.
        let canonicals = std::iter::once((
            parent.program_id(),
            CanonicalSource {
                program: main_program.clone(),
                module: None,
            },
        ))
        .collect();
        Self {
            main_kernel,
            main_program,
            scope_values,
            per_op_kernels: Vec::new(),
            per_op_programs: Vec::new(),
            per_op_side_effecting: Vec::new(),
            scope_value_main_idx,
            scope_value_per_op_idx: Vec::new(),
            needs_broadcast: false,
            canonicals: Arc::new(canonicals),
        }
    }

    /// SRD-68 Push 3 — populate this fiber's per-op kernel slots
    /// from the activity's dispenser registry. Walks each
    /// dispenser, calls `dispenser.canonical_kernel()` to get the
    /// dispenser-owned canonical kernel (when present), and binds a
    /// per-fiber kernel of its program under this fiber's main kernel.
    /// Slot positions match the dispenser registry's order so
    /// cycle-time dispatch can index by `template_idx`. Dispensers
    /// that return `None` (no GK needs) get a `None` slot —
    /// `ExecCtx::wires` falls back to the `NullWireSource` baseline
    /// for those cycles.
    ///
    /// A per-op kernel is instantiated from its op-template module on
    /// the fiber engine when the fiber has one for the canonical's
    /// program, carrying the module's `result:` write-throughs; any
    /// other canonical program runs on the interpreter.
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
        // `fiber.main_kernel` (this fiber's per-fiber scope kernel for
        // the current phase), NOT from the dispenser's shared
        // `canonical_kernel`. The dispenser's canonical_kernel becomes
        // a *program source*, so there is one consistent per-fiber
        // chain:
        //     fiber.main_kernel → per_op_kernel
        // Computed outputs on main_kernel are reachable from
        // per_op_kernel via the standard scope-chain mechanism;
        // per-fiber state (cycle, scope values) propagates
        // correctly without external refresh.
        // A canonical kernel may be on any engine: what it stands for —
        // the interpreter program resolving its indices, and the module
        // a per-op kernel is instantiated from — is looked up by its
        // program's identity. A canonical this activity's builder did
        // not hand out — an adapter's own kernel — stands for its
        // interpreter program when it has one; a compiled one stands for
        // nothing it can bind, and its dispenser runs against the fiber's
        // main kernel.
        let sources: Vec<Option<CanonicalSource>> = dispensers
            .iter()
            .map(|d| {
                let kernel = d.canonical_kernel()?;
                if let Some(source) = self.canonicals.get(&kernel.program_id()) {
                    return Some(source.clone());
                }
                let program = kernel.fork().into_program().as_interpreter();
                if program.is_none() {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "a dispenser's canonical kernel ({}) was not built by this \
                         activity; its op reads the fiber's main kernel",
                        kernel.engine()
                    );
                }
                program.map(|program| CanonicalSource {
                    program,
                    module: None,
                })
            })
            .collect();
        let dispenser_programs: Vec<Option<Arc<PolydatProgram>>> = sources
            .iter()
            .map(|s| s.as_ref().map(|s| s.program.clone()))
            .collect();
        let mut per_op_kernels: Vec<Option<Box<dyn Kernel>>> =
            Vec::with_capacity(dispenser_programs.len());
        let mut per_op_idx: Vec<Option<Vec<Option<usize>>>> =
            Vec::with_capacity(dispenser_programs.len());
        let mut per_op_side_effecting: Vec<Vec<usize>> =
            Vec::with_capacity(dispenser_programs.len());
        for maybe_source in &sources {
            let Some(CanonicalSource { program, module }) = maybe_source else {
                per_op_kernels.push(None);
                per_op_idx.push(None);
                per_op_side_effecting.push(Vec::new());
                continue;
            };
            // Scope values are bound with the kernel, before its consts
            // initialize; the indices are cached for `reset_captures`.
            let bindings = scope_bindings(program, &scope_values);
            let mut op_kernel = match module {
                Some(module) => module
                    .instantiate_under(
                        self.main_kernel.as_ref(),
                        crate::fiber_engine::fiber_engine(),
                        &bindings,
                    )
                    .unwrap_or_else(|e| panic!("per-op kernel failed to instantiate: {e}")),
                None => polydat::kernel::bind_under(
                    self.main_kernel.as_ref(),
                    program.clone() as Arc<dyn KernelProgram>,
                    &bindings,
                )
                .unwrap_or_else(|e| panic!("per-op kernel failed to bind: {e}")),
            };
            let idx_vec: Vec<Option<usize>> = scope_values
                .iter()
                .map(|(name, _)| scope_value_index(program, name))
                .collect();
            for init_name in program.const_outputs() {
                let Some(idx) = program.output_index(init_name) else {
                    continue;
                };
                // Const warmup is best-effort: a const whose freeze
                // fails here stays dirty and re-evals (or fails
                // visibly) at its first per-cycle use. But the failure
                // is never discarded silently — the enriched payload
                // goes to the session log so a broken const is
                // diagnosable before the per-cycle path trips over it.
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    op_kernel.pull_at(idx);
                })) {
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
            let side_effecting = program
                .outputs_with_side_effects()
                .iter()
                .filter_map(|name| program.output_index(name))
                .collect();
            per_op_kernels.push(Some(op_kernel));
            per_op_idx.push(Some(idx_vec));
            per_op_side_effecting.push(side_effecting);
        }
        // SRD-13f Push D: the main kernel publishes its outputs through
        // broadcast cells only when a per-op kernel has a *different*
        // program from it. When the per-op kernel reuses the main
        // program (the flattened-op-template path — no per-op matter),
        // evaluating outputs on main is both redundant (the descendant
        // evaluates the same wires locally) and harmful (side-effecting
        // nodes like `testkit_throw_at` fire outside the per-op cascade
        // surface, losing the panic-to-error pipeline). A per-op kernel
        // with its own program carries `extern <name>` slots for
        // cross-scope wires it doesn't replicate locally; those slots
        // need main to compute and broadcast the values through the
        // cell.
        self.needs_broadcast = dispenser_programs
            .iter()
            .flatten()
            .any(|p| !Arc::ptr_eq(p, &self.main_program));
        self.per_op_kernels = per_op_kernels;
        self.per_op_programs = dispenser_programs;
        self.per_op_side_effecting = per_op_side_effecting;
        self.scope_value_per_op_idx = per_op_idx;
    }

    /// The cycle-time wire surface for the firing dispenser at
    /// `template_idx`: its per-fiber kernel, or this fiber's main
    /// kernel when the dispenser exposes no canonical kernel (the
    /// flattened path).
    pub fn cycle_wires(&mut self, template_idx: usize) -> crate::wires::CycleWires<'_> {
        let program = self
            .per_op_programs
            .get(template_idx)
            .and_then(|p| p.clone());
        match (
            self.per_op_kernels
                .get_mut(template_idx)
                .and_then(|s| s.as_mut()),
            program,
        ) {
            (Some(kernel), Some(program)) => {
                crate::wires::CycleWires::over(kernel.as_mut(), program)
            }
            _ => {
                crate::wires::CycleWires::over(self.main_kernel.as_mut(), self.main_program.clone())
            }
        }
    }

    /// The cycle-time wire surface over this fiber's main kernel.
    pub fn main_wires(&mut self) -> crate::wires::CycleWires<'_> {
        crate::wires::CycleWires::over(self.main_kernel.as_mut(), self.main_program.clone())
    }

    /// Get the per-fiber kernel for the firing dispenser at
    /// `template_idx`. Returns `None` when the dispenser exposes
    /// no canonical kernel (adapters with no Polydat needs); callers
    /// fall back to the `NullWireSource` baseline.
    pub fn per_op_kernel(&self, template_idx: usize) -> Option<&dyn Kernel> {
        self.per_op_kernels
            .get(template_idx)
            .and_then(|s| s.as_deref())
    }

    /// This fiber's main kernel.
    pub fn main_kernel(&self) -> &dyn Kernel {
        self.main_kernel.as_ref()
    }

    /// The interpreter program this fiber's main kernel runs or was
    /// imaged from.
    pub fn program(&self) -> &Arc<PolydatProgram> {
        &self.main_program
    }

    /// Set coordinates and begin a new evaluation scope.
    ///
    /// Bounded by each kernel's coordinate count: the slice is
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
        let main_n = coords.len().min(self.main_kernel.coord_count());
        if main_n > 0 {
            self.main_kernel.set_inputs(&coords[..main_n]);
        }
        for kernel in self.per_op_kernels.iter_mut().flatten() {
            let n = coords.len().min(kernel.coord_count());
            if n > 0 {
                kernel.set_inputs(&coords[..n]);
            }
        }
    }

    /// Feed a source item into the fiber's kernels.
    ///
    /// Sets the ordinal as the coordinate input and injects field
    /// projections into the appropriate input slots (e.g.
    /// `base__ordinal`, `base__vector`). The ordinal write is
    /// skipped by kernels whose programs declare no coordinates
    /// (only externs and stanza-invariant bindings) rather than
    /// clobbering an extern slot. Field projections always write by
    /// name, so they're safe regardless of coordinate count.
    ///
    /// SRD-13d Phase 9: ordinal + fields propagate to every
    /// op-template kernel that declares matching slots.
    pub fn set_source_item(&mut self, item: &polydat::iteration::source::SourceItem) {
        if self.main_kernel.coord_count() > 0 {
            self.main_kernel.set_inputs(&[item.ordinal]);
        }
        // Source items carry typed values from their upstream
        // DataSource; one the slot's type cannot take, converted or
        // not, means the source produced an incompatible value, which
        // is a fail-loud condition.
        for (name, value) in &item.fields {
            if let Some(idx) = self.main_program.find_input(name) {
                crate::wires::write_input(self.main_kernel.as_mut(), idx, name, value.clone())
                    .unwrap_or_else(|e| {
                        panic!("source item field '{name}' failed typed write: {e}")
                    });
            }
        }
        // Cell-bound cross-fiber visibility is substrate-owned via the
        // per-cell revision counter + per-scope intent-dirty vector
        // (polydat/docs/design/cross_fiber_invalidation.md): ancestor
        // writes through a `SharedCell` bump the cell's revision and
        // set its intent bit, and the evaluator re-checks both before
        // serving a memoized result. No host-side refresh call needed.
        for (kernel, program) in self
            .per_op_kernels
            .iter_mut()
            .zip(self.per_op_programs.iter())
        {
            let (Some(kernel), Some(program)) = (kernel, program) else {
                continue;
            };
            if kernel.coord_count() > 0 {
                kernel.set_inputs(&[item.ordinal]);
            }
            for (name, value) in &item.fields {
                if let Some(idx) = program.find_input(name) {
                    crate::wires::write_input(kernel.as_mut(), idx, name, value.clone())
                        .unwrap_or_else(|e| {
                            panic!("source item field '{name}' failed typed write: {e}")
                        });
                }
            }
        }
        if self.needs_broadcast {
            self.main_kernel.publish_broadcasts();
        }
    }

    /// Reset capture inputs to defaults. Called at stanza
    /// boundaries to prevent capture leakage across stanzas.
    /// Coordinates and cell-bound slots are not reset. Scope-bound
    /// iter-var inputs (set by [`OpBuilder::create_fiber_builder`])
    /// are re-applied after the reset so the iteration's bound
    /// values survive the boundary.
    pub fn reset_captures(&mut self) {
        self.main_kernel.reset_inputs();
        for ((name, value), idx_opt) in self
            .scope_values
            .iter()
            .zip(self.scope_value_main_idx.iter())
        {
            if let Some(idx) = idx_opt {
                crate::wires::write_input(self.main_kernel.as_mut(), *idx, name, value.clone())
                    .unwrap_or_else(|e| panic!("scope value '{name}' failed typed write: {e}"));
            }
        }
        for (slot, idx_slot) in self
            .per_op_kernels
            .iter_mut()
            .zip(self.scope_value_per_op_idx.iter())
        {
            if let (Some(kernel), Some(idx_vec)) = (slot, idx_slot) {
                kernel.reset_inputs();
                for ((name, value), idx_opt) in self.scope_values.iter().zip(idx_vec.iter()) {
                    if let Some(idx) = idx_opt {
                        crate::wires::write_input(kernel.as_mut(), *idx, name, value.clone())
                            .unwrap_or_else(|e| {
                                panic!("scope value '{name}' failed typed write: {e}")
                            });
                    }
                }
            }
        }
    }

    /// Invalidate all state: every step, a side channel included,
    /// runs again when next pulled. Provides "clean slate" semantics.
    pub fn invalidate_all(&mut self) {
        self.main_kernel.invalidate_all();
        for kernel in self.per_op_kernels.iter_mut().flatten() {
            kernel.invalidate_all();
        }
    }

    /// Store a captured value into the main kernel's input slot `name`.
    /// Returns `true` when the slot exists and took the value, `false`
    /// when the program has no such input or the value could not be
    /// converted to its type (value dropped).
    pub fn capture(&mut self, name: &str, value: Value) -> bool {
        match self.main_program.find_input(name) {
            Some(idx) => {
                crate::wires::write_input(self.main_kernel.as_mut(), idx, name, value).is_ok()
            }
            None => false,
        }
    }

    /// SRD-68 Push 5d: position-indexed write into the per-fiber
    /// op-template kernel slot. Used by the cycle dispatch's
    /// post-execute capture flow to feed result-binding inputs
    /// (`body` / `count` / `ok` and any captures) into the kernel
    /// before [`Self::commit_op_template_write_throughs_for_idx`]
    /// fans the computed values up through parent `shared` cells.
    ///
    /// No-op returning `false` when (a) the op didn't materialise a
    /// kernel (flattened op-template), or (b) the kernel doesn't
    /// declare an input slot for `name` (the closure-binding economy
    /// dropped it because the source doesn't reference it), or (c)
    /// the value could not be converted to the slot's type.
    pub fn write_op_template_input_for_idx(
        &mut self,
        template_idx: usize,
        name: &str,
        value: Value,
    ) -> bool {
        let (Some(Some(kernel)), Some(Some(program))) = (
            self.per_op_kernels.get_mut(template_idx),
            self.per_op_programs.get(template_idx),
        ) else {
            if nmbrs_dirty_debug_enabled() && name == "body" {
                eprintln!("DIRTY: write body template={template_idx} NO_KERNEL");
            }
            return false;
        };
        let Some(idx) = program.find_input(name) else {
            if nmbrs_dirty_debug_enabled() && name == "body" {
                let inputs = program.input_names();
                eprintln!(
                    "DIRTY: write body template={template_idx} NO_SLOT in_count={} names={:?}",
                    inputs.len(),
                    inputs
                );
            }
            return false;
        };
        if nmbrs_dirty_debug_enabled() && name == "body" {
            let display = value.to_display_string();
            let head: String = display.chars().take(48).collect();
            eprintln!(
                "DIRTY: write body template={template_idx} idx={idx} in_count={} \
                 head=\"{head}\"",
                program.input_names().len()
            );
        }
        crate::wires::write_input(kernel.as_mut(), idx, name, value).is_ok()
    }

    /// SRD-68 Push 5d: position-indexed Rule 2 write-through commit.
    /// Pulls every `__write_<X>` and stores its value through the
    /// cell-bound input slot for `<X>`, propagating each result-
    /// binding LHS value to the parent's `SharedCell` (and from
    /// there to any sibling phase that imports the same name).
    /// No-op when the kernel carries no write-throughs (typical
    /// for ops without `result:`).
    ///
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
        let (Some(indices), Some(Some(kernel))) = (
            self.per_op_side_effecting.get(template_idx),
            self.per_op_kernels.get_mut(template_idx),
        ) else {
            return;
        };
        for &idx in indices {
            let _ = kernel.pull_at(idx);
        }
    }

    /// Materialize a [`PullPlan`] against this fiber's main kernel.
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
        plan.resolve(self.main_kernel.as_mut())
    }

    /// SRD-68 Push 5d resolve path — picks the right kernel for the
    /// dispenser at `template_idx` and resolves the plan against it.
    /// When a per-fiber op-template kernel was instanced for that
    /// position (every adapter exposes `canonical_kernel()` so this is
    /// the typical case), it is used; otherwise the plan resolves
    /// against the fiber's main kernel (the flattened op-template path).
    /// Either way the plan is checked against the interpreter program
    /// the kernel was imaged from, whose indices it holds.
    pub fn resolve_pulls_for_idx(
        &mut self,
        template_idx: usize,
        plan: &crate::fixture::PullPlan,
    ) -> crate::fixture::ResolvedPulls {
        let program = self
            .per_op_programs
            .get(template_idx)
            .and_then(|p| p.clone());
        match (
            self.per_op_kernels
                .get_mut(template_idx)
                .and_then(|s| s.as_mut()),
            program,
        ) {
            (Some(kernel), Some(program)) => {
                plan.check_program_match(&program, template_idx);
                plan.resolve(kernel.as_mut())
            }
            _ => {
                plan.check_program_match(&self.main_program, template_idx);
                plan.resolve(self.main_kernel.as_mut())
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
        let canonical_kernel: std::sync::Arc<dyn polydat::Kernel> =
            builder.canonical_kernel_for_op("nonexistent");
        // For this probe we only need the canonical to expose
        // a program; reuse builder's source_kernel program.
        let _ = canonical_program;

        struct ProbeDispenser(std::sync::Arc<dyn polydat::Kernel>);
        impl OpDispenser for ProbeDispenser {
            fn canonical_kernel(&self) -> Option<&std::sync::Arc<dyn polydat::Kernel>> {
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
                per_op_a as *const dyn Kernel as *const (),
                canonical_kernel.as_ref() as *const dyn polydat::Kernel as *const ()
            ),
            "per_op_a must be a distinct per-fiber instance, \
             not the shared canonical",
        );
        assert!(
            !std::ptr::eq(
                per_op_b as *const dyn Kernel as *const (),
                canonical_kernel.as_ref() as *const dyn polydat::Kernel as *const ()
            ),
            "per_op_b must be a distinct per-fiber instance, \
             not the shared canonical",
        );
        assert!(
            !std::ptr::eq(
                per_op_a as *const dyn Kernel as *const (),
                per_op_b as *const dyn Kernel as *const ()
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
        // should trigger an eval — the post-fold leaf-const path
        // returns the constant directly.
        for _ in 0..32 {
            let mut fiber = builder.create_fiber_builder();
            fiber.set_inputs(&[0]);
            let pulled = {
                use crate::wires::WireSource as _;
                fiber.main_wires().get("ticks").expect("ticks resolves")
            };
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

    /// The fiber engine carries the per-cycle kernels: a fiber's main
    /// kernel runs the phase program's image, and a per-op kernel is
    /// instantiated from its op-template module, both off the
    /// interpreter — and every value they compute is the interpreter's.
    #[test]
    fn fiber_kernels_run_on_the_fiber_engine_with_interpreter_values() {
        use crate::adapter::OpDispenser;
        use crate::wires::WireSource as _;
        use polydat::kernel::subcontext::{BodyFragment, PolydatMatter, SubcontextBuilder};

        let phase_src = "input cycle: u64
h := mod(hash(cycle), 1000)
";
        let options = polydat::dsl::compile::CompileOptions::default();
        let phase = crate::bindings::compile_scope_kernel(phase_src, &options).expect("phase");
        let image = crate::fiber_engine::source_image(phase.program(), phase_src, &options, "test")
            .expect("the phase image agrees with its program");
        let phase = Arc::new(phase);

        let mut op = SubcontextBuilder::under(phase.as_ref());
        op.body(BodyFragment::PolydatSource(
            "extern h: u64
scaled := h * 3 + 1
"
            .into(),
        ));
        let module = Arc::new(op.finalize().expect("op-template module"));
        let canonical: Arc<dyn polydat::Kernel> = Arc::new(
            phase
                .build_subscope(
                    PolydatMatter::builder()
                        .program(module.program().clone())
                        .build()
                        .expect("program matter"),
                )
                .expect("canonical per-op kernel"),
        );

        struct Probe(Arc<dyn polydat::Kernel>);
        impl OpDispenser for Probe {
            fn canonical_kernel(&self) -> Option<&Arc<dyn polydat::Kernel>> {
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
        let dispensers: Vec<Arc<dyn OpDispenser>> = vec![Arc::new(Probe(canonical))];

        let native = OpBuilder::new(phase.clone())
            .with_fiber_image(Some(image))
            .with_op_template_modules([("op".to_string(), module)]);
        let interpreted = OpBuilder::new(phase);
        let mut fiber = native.create_fiber_builder();
        fiber.attach_dispenser_kernels(&dispensers);
        let mut reference = interpreted.create_fiber_builder();
        reference.attach_dispenser_kernels(&dispensers);

        let on_interpreter = |k: &dyn Kernel| matches!(k.engine(), polydat::Engine::Interpreter(_));
        assert!(
            !on_interpreter(fiber.main_kernel()),
            "main kernel on {}",
            fiber.main_kernel().engine()
        );
        let per_op = fiber.per_op_kernel(0).expect("per-op kernel attached");
        assert!(
            !on_interpreter(per_op),
            "per-op kernel on {}",
            per_op.engine()
        );
        assert!(on_interpreter(reference.main_kernel()));
        assert!(on_interpreter(
            reference.per_op_kernel(0).expect("reference per-op")
        ));

        for cycle in [0, 1, 7, 1_000_003] {
            fiber.set_inputs(&[cycle]);
            reference.set_inputs(&[cycle]);
            fiber.set_source_item(&polydat::iteration::source::SourceItem {
                ordinal: cycle,
                fields: Vec::new(),
            });
            reference.set_source_item(&polydat::iteration::source::SourceItem {
                ordinal: cycle,
                fields: Vec::new(),
            });
            for name in ["h", "scaled"] {
                assert_eq!(
                    fiber.cycle_wires(0).get(name),
                    reference.cycle_wires(0).get(name),
                    "'{name}' at cycle {cycle}"
                );
            }
        }
    }
}
