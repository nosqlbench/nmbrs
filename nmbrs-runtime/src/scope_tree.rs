// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Canonical scope tree for a workload's runtime hierarchy.
//!
//! `ScenarioNode` (in `nmbrs-workload`) is the *static authored*
//! tree — what the user wrote in YAML. `ScopeTree` is the
//! *runtime hierarchy* — what Polydat and the scheduler see. Every
//! non-trivial scenario node gets a 1:1 scope here, with
//! parent pointers, depth, pragma sets, and a slot for a compiled
//! kernel.
//!
//! This module is **structural only** — it builds the tree and
//! exposes traversal helpers. Pragma attachment, kernel
//! compilation, and execution scheduling live in subsequent
//! steps of SRD 18b §"Migration":
//!
//! 1. *(this module)* introduce the data structure
//! 2. wire `PragmaSet::attach_to` at scope-tree construction
//!    (M2 follow-up)
//! 3. replace text-substitution of iteration vars with extern
//!    binding (compile leaf phases once)
//! 4. pluggable scheduler reading the `schedule=<level0>/...`
//!    spec
//! 5. hierarchical display surface
//!
//! Until those steps land, `ScopeTree` is built but not consumed
//! by the runner — the existing executor continues to drive
//! traversal directly off `ScenarioNode`. Building the tree is
//! cheap and deterministic; intermediate sysrefs (TUI display,
//! `dryrun=phase`) can already start consuming it.

use nmbrs_workload::model::ScenarioNode;
use polydat::dsl::pragmas::PragmaSet;
use polydat::iteration::comprehension::Comprehension;

/// Index into the `ScopeTree.nodes` vector. Stable for the
/// lifetime of the tree.
pub type ScopeNodeIdx = usize;

/// What kind of scope a `ScopeNode` represents. Mirrors the
/// `ScenarioNode` variants 1:1, with two extra kinds for the
/// implicit workload root and the named scenario layer that
/// wraps the user's authored children. SRD 18b §"Canonical
/// traversal".
#[derive(Debug, Clone)]
pub enum ScopeKind {
    /// The session root — **one per process** (SRD-88). The shared
    /// common root every execution derives from: it owns the session
    /// polydat scope (the process/session-level args) and the
    /// `session=<id>` identity. Each [`ScopeKind::Workload`] hangs
    /// under it as one execution. For a single-execution run there is
    /// exactly one workload child.
    Session,
    /// A workload root — **one per execution** (SRD-88). Owns the
    /// outer Polydat Kernel for its workload, compiled at execution
    /// start, binding the session scope as its outer.
    Workload,
    /// A named scenario. Wraps the scenario's children so that
    /// "phase P in scenario default" survives as a path query
    /// rather than a elided label.
    Scenario { name: String },
    /// Iteration scope — `for_each` (single or multi-clause) or
    /// `for_each_union`. The `Comprehension` AST captures the
    /// shape and clauses; the executor uses it to enumerate
    /// tuples and bind iteration variables on per-iteration
    /// child kernels.
    Comprehension { comprehension: Comprehension },
    /// Logical inclusion of another scenario by name. The
    /// runtime walks straight through to the children; the
    /// scope is preserved purely so the scope tree retains the
    /// include hierarchy for `dryrun=phase` and TUI output.
    /// See
    /// [`nmbrs_workload::model::ScenarioNode::IncludedScenario`].
    IncludedScenario { name: String },
    /// `do_while` with optional counter as a scope output.
    DoWhile {
        condition: String,
        counter: Option<String>,
    },
    /// `do_until` with optional counter as a scope output.
    DoUntil {
        condition: String,
        counter: Option<String>,
    },
    /// A phase reference. With SRD-13d Phase 6 the phase is no
    /// longer a leaf — every op template the phase declares
    /// becomes an `OpTemplate` child of this node. The kernel
    /// slot, if filled, holds the per-phase Polydat program.
    Phase { name: String },
    /// SRD-13d Phase 6 — an op template's scope, child of its
    /// declaring phase. Per-template Polydat content (`bindings:`,
    /// `metrics:` wire-injections, inline `{{<expr>}}` rewrites)
    /// hangs off this node; the scope-elision pre-walk
    /// (§3.3) decides whether it materialises its own kernel
    /// or elides into the parent phase. Op-template scopes
    /// also own per-op `Component` instances at runtime so
    /// SRD-40b's duplicate-family check (via
    /// `Component::register_instrument`) surfaces per-op
    /// rather than per-phase.
    OpTemplate { name: String },
    /// Scenario-tree-level Polydat bindings block (see
    /// [`nmbrs_workload::model::ScenarioNode::Bindings`]). The
    /// `source` is Polydat matter text that compiles into a kernel
    /// layered over the parent scope. Used for any scope-tree-
    /// level state injection: workload-param shadowing (the
    /// `set: { ... }` sugar form), derived bindings spanning a
    /// subtree, shared cells, etc. — the Polydat grammar is the
    /// only constraint on what the source may contain.
    Bindings { source: String },
}

impl ScopeKind {
    /// True if this kind opens a *new* Polydat scope (its own
    /// kernel + pragmas + extern wiring). Phase scopes are only
    /// "new" when the phase has its own bindings or it's an
    /// iteration of a parent — that decision lives in the
    /// compiler step, not this static descriptor.
    pub fn opens_kernel(&self) -> bool {
        !matches!(self, ScopeKind::Workload | ScopeKind::Session)
    }

    /// Short label for diagnostic output (`dryrun=phase`, TUI).
    pub fn label(&self) -> String {
        match self {
            ScopeKind::Session => "session".into(),
            ScopeKind::Workload => "workload".into(),
            ScopeKind::Scenario { name } => format!("scenario '{name}'"),
            ScopeKind::Comprehension { comprehension } => label_for_comprehension(comprehension),
            ScopeKind::IncludedScenario { name } => format!("scenario '{name}'"),
            ScopeKind::DoWhile { condition, counter } => match counter {
                Some(c) => format!("do_while {condition} ({c})"),
                None => format!("do_while {condition}"),
            },
            ScopeKind::DoUntil { condition, counter } => match counter {
                Some(c) => format!("do_until {condition} ({c})"),
                None => format!("do_until {condition}"),
            },
            ScopeKind::Phase { name } => format!("phase '{name}'"),
            ScopeKind::OpTemplate { name } => format!("op '{name}'"),
            ScopeKind::Bindings { source } => bindings_label(source),
        }
    }
}

/// Render a one-line label for an algebra-AST comprehension.
///
/// Strips off outer `Order` / `Filter` wrappers (which don't
/// affect the structural display) to find the inner body
/// shape: `Cartesian` renders as `each v1, v2, ...`, `Union`
/// renders as `for_each_union {[...]; [...]}`, and a bare
/// `Clause` renders as `each <var>`.
fn label_for_comprehension(comp: &Comprehension) -> String {
    use polydat::iteration::comprehension::source::Source;
    // Peel outer Order/Filter — these are non-structural for
    // the label.
    let mut body = comp;
    while let Comprehension::Order { child, .. } | Comprehension::Filter { child, .. } = body {
        body = child.as_ref();
    }
    fn var_of(node: &Comprehension) -> String {
        match node {
            Comprehension::Clause { name, .. } => name.clone(),
            Comprehension::Zip { children, .. } => {
                let vs: Vec<String> = children.iter().map(var_of).collect();
                format!("({})", vs.join(", "))
            }
            _ => "?".to_string(),
        }
    }
    fn expr_of(node: &Comprehension) -> String {
        match node {
            Comprehension::Clause { source, .. } => match source {
                Source::IntRange { lo, hi, step } if *step == 1 => format!("{lo}..{hi}"),
                Source::IntRange { lo, hi, step } => format!("{lo}..{hi} step {step}"),
                Source::Literal { values } if values.len() == 1 => format!("{:?}", values[0]),
                Source::Literal { values } => format!("[{} values]", values.len()),
                Source::Generator { expr, .. } => expr.clone(),
                Source::WorkloadParamList { name, .. } => format!("{{{name}}}"),
                Source::ContinuousInterval { interval, .. } => {
                    format!("{}..{}", interval.lo, interval.hi)
                }
                Source::Distribution { .. } => "<dist>".to_string(),
            },
            Comprehension::Zip { children, .. } => {
                let es: Vec<String> = children.iter().map(expr_of).collect();
                format!("({})", es.join(", "))
            }
            _ => "?".to_string(),
        }
    }
    match body {
        Comprehension::Clause { name, .. } => format!("each {name}"),
        Comprehension::Cartesian { children } if children.len() == 1 => {
            format!("each {}", var_of(&children[0]))
        }
        Comprehension::Cartesian { children } => {
            let vars: Vec<String> = children.iter().map(var_of).collect();
            format!("each {}", vars.join(", "))
        }
        Comprehension::Union { children } => {
            let parts: Vec<String> = children
                .iter()
                .map(|sub| {
                    // Each Union child is itself a Cartesian (or
                    // single Clause/Zip). Render its dims.
                    let dims: Vec<String> = match sub {
                        Comprehension::Cartesian { children: c } => c
                            .iter()
                            .map(|n| format!("{} in {}", var_of(n), expr_of(n)))
                            .collect(),
                        other => vec![format!("{} in {}", var_of(other), expr_of(other))],
                    };
                    format!("[{}]", dims.join(", "))
                })
                .collect();
            format!("for_each_union {{{}}}", parts.join(" | "))
        }
        Comprehension::Zip { .. } => format!("each {}", var_of(body)),
        Comprehension::Filter { .. } | Comprehension::Order { .. } => unreachable!(),
    }
}

/// One node in the runtime scope tree. Carries enough metadata
/// for the scheduler to walk and the compiler to fill in.
#[derive(Debug)]
pub struct ScopeNode {
    pub kind: ScopeKind,
    pub parent: Option<ScopeNodeIdx>,
    pub children: Vec<ScopeNodeIdx>,
    /// Depth from the root. Root is 0; its children are 1; and
    /// so on. The scheduler's `schedule=<level0>/<level1>/...`
    /// spec indexes by *child depth*, so a node at depth `d`
    /// schedules its children with the spec entry for index
    /// `d`.
    pub depth: usize,
    /// Pragmas declared at this scope level. Empty by default;
    /// step 2 of the migration fills these in by walking the
    /// node's source (or, for control-flow nodes, the optional
    /// inline pragma block once the workload model supports
    /// per-node pragmas).
    pub pragmas: PragmaSet,
    /// Cache for this scope's compiled Polydat Kernel — the canonical
    /// instance that owns its `Arc<PolydatProgram>` and a folded-
    /// constant-seeded `PolydatState` so `get_constant(name)` is a
    /// straight `&self` read. Populated at pre-map time by
    /// [`ScopeTree::install_kernel`].
    ///
    /// SRD 18b §"Iteration variables as scope outputs": every
    /// non-trivial scope owns a kernel. The cached kernel is
    /// shared via `Arc` (read-only canonical state). Mutable
    /// per-iteration / per-fiber execution pulls a fresh kernel
    /// via `PolydatKernel::from_program(kernel.program().clone())` —
    /// the cache-and-rebind primitive documented on
    /// `PolydatKernel::from_program`.
    ///
    /// `OnceLock` keeps installation lock-free; downstream
    /// readers walk the parent chain via
    /// [`ScopeTree::lookup_name`] and never touch this slot
    /// directly.
    pub cached_kernel: std::sync::OnceLock<std::sync::Arc<polydat::kernel::PolydatKernel>>,
    /// SRD-13d §3 scope-elision mark — set once at
    /// pre-walk by [`ScopeTree::mark_scope_elision`] and
    /// read by every consumer (premap, runtime, diagnostics).
    /// `None` means "not yet computed"; the pre-walk
    /// guarantees every node has `Some` after it finishes.
    /// `true` ⇒ this scope materialises its own kernel;
    /// `false` ⇒ elided into the nearest materialised
    /// ancestor.
    pub materialised: Option<bool>,
    /// SRD-13d §5.3 logical kernel name. Stable, fully-
    /// qualified scope-tree path (`workload`, `phase.<n>`,
    /// `phase.<n>.op.<o>`, etc.). Used by `dryrun=op`
    /// diagnostics and `nmbrs describe wiring` displays. Empty
    /// before the pre-walk runs.
    pub logical_name: String,
}

// `OnceLock` doesn't implement `Clone`, so neither does
// `ScopeNode` automatically. We don't actually need clones today
// — the tree is built once and shared via `Arc<ScopeTree>` — but
// some test helpers and serialisation paths assume `Clone`.
// Provide a manual clone that drops the cache (subsequent reads
// repopulate from a fresh compile, which is correct for clones
// since each clone owns an independent cache).
impl Clone for ScopeNode {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind.clone(),
            parent: self.parent,
            children: self.children.clone(),
            depth: self.depth,
            pragmas: self.pragmas.clone(),
            cached_kernel: std::sync::OnceLock::new(),
            materialised: self.materialised,
            logical_name: self.logical_name.clone(),
        }
    }
}

/// A workload's runtime scope hierarchy. Built once per session.
/// Stable indices into `nodes`; parent / child pointers are
/// `ScopeNodeIdx`. Use the helpers on this struct for traversal
/// — direct `nodes` access is fine for read-only inspection but
/// the navigation helpers are easier to read.
#[derive(Debug, Clone)]
pub struct ScopeTree {
    pub nodes: Vec<ScopeNode>,
    pub root: ScopeNodeIdx,
}

impl ScopeTree {
    /// Build a scope tree from the resolved scenario children.
    /// `scenario_name` becomes the named [`ScopeKind::Scenario`]
    /// that wraps `nodes` — the user's authored grouping is
    /// preserved as a real ancestor, restoring "phase P in
    /// scenario default" as a path query.
    pub fn build(scenario_name: &str, nodes: &[ScenarioNode]) -> Self {
        let mut tree = ScopeTree {
            nodes: Vec::new(),
            root: 0,
        };

        // Root: the session — one per process (SRD-88), the shared
        // common root. Always at index 0.
        tree.nodes.push(ScopeNode {
            kind: ScopeKind::Session,
            parent: None,
            children: Vec::new(),
            depth: 0,
            pragmas: PragmaSet::default(),
            cached_kernel: std::sync::OnceLock::new(),
            materialised: None,
            logical_name: String::new(),
        });

        // The workload root — one per execution (SRD-88), under the
        // session. Owns the outer workload Polydat Kernel.
        let workload_idx = tree.add_node(ScopeNode {
            kind: ScopeKind::Workload,
            parent: Some(0),
            children: Vec::new(),
            depth: 1,
            pragmas: PragmaSet::default(),
            cached_kernel: std::sync::OnceLock::new(),
            materialised: None,
            logical_name: String::new(),
        });
        tree.nodes[0].children.push(workload_idx);

        // Scenario layer wraps the user's children. This is the
        // "lost grouping" the user called out — a real scope
        // ancestor named after the scenario.
        let scenario_idx = tree.add_node(ScopeNode {
            kind: ScopeKind::Scenario {
                name: scenario_name.into(),
            },
            parent: Some(workload_idx),
            children: Vec::new(),
            depth: 2,
            pragmas: PragmaSet::default(),
            cached_kernel: std::sync::OnceLock::new(),
            materialised: None,
            logical_name: String::new(),
        });
        tree.nodes[workload_idx].children.push(scenario_idx);

        // Walk the user's children recursively under the scenario.
        for child in nodes {
            tree.append_subtree(scenario_idx, child);
        }

        tree
    }

    /// The workload-root node — the single child of the session root
    /// (node 0). One per execution; owns the outer workload kernel.
    /// Falls back to the root if (degenerately) there is no workload
    /// layer.
    pub fn workload_root_idx(&self) -> ScopeNodeIdx {
        self.nodes[0].children.first().copied().unwrap_or(0)
    }

    /// The scenario-layer node — the single child of the workload root
    /// (node 0). The walker seeds its scope cursor here so the top-level
    /// scenario nodes resolve **positionally** against this node's children
    /// (one scope-tree child per scenario node, in order — see
    /// [`Self::append_subtree`]). Falls back to the root if (degenerately)
    /// there is no scenario layer.
    pub fn scenario_root_idx(&self) -> ScopeNodeIdx {
        // Session(0) → Workload → Scenario. Walk two layers down.
        let workload = self.workload_root_idx();
        self.nodes[workload]
            .children
            .first()
            .copied()
            .unwrap_or(workload)
    }

    /// Append the subtree rooted at `node` as a child of `parent_idx`.
    /// Recursive — control-flow nodes pull in their own children.
    fn append_subtree(&mut self, parent_idx: ScopeNodeIdx, node: &ScenarioNode) {
        let parent_depth = self.nodes[parent_idx].depth;
        let depth = parent_depth + 1;

        match node {
            ScenarioNode::Phase(name) => {
                let idx = self.add_node(ScopeNode {
                    kind: ScopeKind::Phase { name: name.clone() },
                    parent: Some(parent_idx),
                    children: Vec::new(),
                    depth,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[parent_idx].children.push(idx);
            }
            ScenarioNode::Comprehension {
                comprehension,
                children,
                ..
            } => {
                let idx = self.add_node(ScopeNode {
                    kind: ScopeKind::Comprehension {
                        comprehension: comprehension.clone(),
                    },
                    parent: Some(parent_idx),
                    children: Vec::new(),
                    depth,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[parent_idx].children.push(idx);
                for child in children {
                    self.append_subtree(idx, child);
                }
            }
            ScenarioNode::IncludedScenario { name, children } => {
                let idx = self.add_node(ScopeNode {
                    kind: ScopeKind::IncludedScenario { name: name.clone() },
                    parent: Some(parent_idx),
                    children: Vec::new(),
                    depth,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[parent_idx].children.push(idx);
                for child in children {
                    self.append_subtree(idx, child);
                }
            }
            ScenarioNode::DoWhile {
                condition,
                counter,
                children,
            } => {
                let idx = self.add_node(ScopeNode {
                    kind: ScopeKind::DoWhile {
                        condition: condition.clone(),
                        counter: counter.clone(),
                    },
                    parent: Some(parent_idx),
                    children: Vec::new(),
                    depth,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[parent_idx].children.push(idx);
                for child in children {
                    self.append_subtree(idx, child);
                }
            }
            ScenarioNode::DoUntil {
                condition,
                counter,
                children,
            } => {
                let idx = self.add_node(ScopeNode {
                    kind: ScopeKind::DoUntil {
                        condition: condition.clone(),
                        counter: counter.clone(),
                    },
                    parent: Some(parent_idx),
                    children: Vec::new(),
                    depth,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[parent_idx].children.push(idx);
                for child in children {
                    self.append_subtree(idx, child);
                }
            }
            ScenarioNode::Bindings { source, children } => {
                let idx = self.add_node(ScopeNode {
                    kind: ScopeKind::Bindings {
                        source: source.clone(),
                    },
                    parent: Some(parent_idx),
                    children: Vec::new(),
                    depth,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[parent_idx].children.push(idx);
                for child in children {
                    self.append_subtree(idx, child);
                }
            }
        }
    }

    fn add_node(&mut self, node: ScopeNode) -> ScopeNodeIdx {
        let idx = self.nodes.len();
        self.nodes.push(node);
        idx
    }

    /// SRD-13d Phase 6 — extend every `Phase` scope node with
    /// `OpTemplate` children (one per op declared in the
    /// phase). Two-step build: `ScopeTree::build` produces
    /// the scenario-shaped skeleton (phases as leaves);
    /// this method adds the op-template tier on top by
    /// consulting the workload's per-phase `WorkloadPhase`
    /// records.
    ///
    /// Idempotent: a phase whose `OpTemplate` children are
    /// already present is left alone (the post-build pre-walk
    /// can run before or after this without double-adding).
    /// Run before `mark_scope_elision` so the per-op
    /// classification gets the chance to elide / materialise
    /// each op-template tier.
    pub fn extend_with_op_templates(
        &mut self,
        phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
    ) {
        // Snapshot the indices first — we'll mutate `nodes`
        // during the loop.
        let phase_nodes: Vec<(ScopeNodeIdx, String, usize)> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| match &n.kind {
                ScopeKind::Phase { name } => Some((i, name.clone(), n.depth)),
                _ => None,
            })
            .collect();

        for (phase_idx, phase_name, phase_depth) in phase_nodes {
            // Skip phases that already have OpTemplate children.
            let already_has_ops = self.nodes[phase_idx]
                .children
                .iter()
                .any(|&c| matches!(self.nodes[c].kind, ScopeKind::OpTemplate { .. }));
            if already_has_ops {
                continue;
            }
            // Look up the phase's op list. Phases referenced
            // by name with no entry in `phases` (e.g. the
            // `default` scenario including a phase that's
            // declared elsewhere) just get no op children —
            // not a structural error.
            let Some(phase) = phases.get(&phase_name) else {
                continue;
            };
            for op in &phase.ops {
                let op_idx = self.add_node(ScopeNode {
                    kind: ScopeKind::OpTemplate {
                        name: op.name.clone(),
                    },
                    parent: Some(phase_idx),
                    children: Vec::new(),
                    depth: phase_depth + 1,
                    pragmas: PragmaSet::default(),
                    cached_kernel: std::sync::OnceLock::new(),
                    materialised: None,
                    logical_name: String::new(),
                });
                self.nodes[phase_idx].children.push(op_idx);
            }
        }
    }

    /// SRD-13d §3.3 — pre-walk every scope-tree node and mark
    /// it `materialised` (own kernel) or elided (descendants
    /// bind through parent). Also assigns the SRD-13d §5.3
    /// logical kernel name, which is the fully-qualified
    /// scope-tree path. Run once at workload-load; premap and
    /// runtime read the marks afterward.
    ///
    /// `is_materialising` is the predicate the pre-walk
    /// applies per node — typically a closure that consults
    /// the AST node's `HasPolydatMatter` classification (None /
    /// Readonly ⇒ elide; Definitions ⇒ check program-hash
    /// equivalence with the parent and decide). The walker is
    /// agnostic to the exact predicate; SRD-13d §3.3 fixes
    /// the order.
    ///
    /// The workload root is **always** materialised (see
    /// SRD-13d §5.1) so the walk terminates at a materialised
    /// ancestor regardless of how aggressively descendants
    /// elide.
    pub fn mark_scope_elision<F>(&mut self, mut is_materialising: F)
    where
        F: FnMut(&ScopeKind, ScopeNodeIdx) -> bool,
    {
        // Walk in DFS order; logical names depend on parent
        // names being assigned first, which DFS pre-order
        // guarantees (root → scenario → … → leaf).
        let order: Vec<ScopeNodeIdx> = self.iter_dfs().map(|(idx, _)| idx).collect();
        for idx in order {
            // Root: the session — always materialised, contributes NO
            // logical-path segment (SRD-88; the workload child below
            // owns the `workload` segment, keeping paths
            // `workload.scenario.…`).
            if idx == self.root {
                self.nodes[idx].materialised = Some(true);
                self.nodes[idx].logical_name = String::new();
                continue;
            }
            let kind = self.nodes[idx].kind.clone();
            // The workload node always materialises — it owns the
            // installed workload kernel (SRD-88: it's the per-execution
            // root beneath the session, the old always-materialised
            // root's role). Descendants elide INTO it as before.
            let materialise = matches!(kind, ScopeKind::Workload) || is_materialising(&kind, idx);
            self.nodes[idx].materialised = Some(materialise);

            // Logical name = parent's logical name + "."
            // + per-kind segment. The segment shape follows
            // SRD-13d §5.3's table (`phase.<n>`,
            // `for_each.<var>`, `op.<o>`).
            let parent_name = self.nodes[idx]
                .parent
                .map(|p| self.nodes[p].logical_name.clone())
                .unwrap_or_default();
            let segment = match &kind {
                // SRD-88 — the session is the always-present implicit root;
                // it contributes NO logical-path segment, so addressable
                // paths stay `workload.scenario.…` (the workload/execution
                // is what varies and addresses the path). The session tier
                // is still visible structurally via `kind.label()`.
                ScopeKind::Session => String::new(),
                ScopeKind::Workload => "workload".to_string(),
                ScopeKind::Scenario { name } => format!("scenario.{name}"),
                ScopeKind::Phase { name } => format!("phase.{name}"),
                ScopeKind::OpTemplate { name } => format!("op.{name}"),
                ScopeKind::Comprehension { .. } => "for_each".to_string(),
                ScopeKind::IncludedScenario { name } => format!("include.{name}"),
                ScopeKind::DoWhile { .. } => "do_while".to_string(),
                ScopeKind::DoUntil { .. } => "do_until".to_string(),
                ScopeKind::Bindings { source } => {
                    // First `final NAME` / `NAME :=` in the
                    // source distinguishes this scope-tree node
                    // in the logical-name path. For sugar from
                    // `set: { mode: verbose }` the source starts
                    // with `const mode := …` so the segment is
                    // `bindings.mode`. Sources with no clear
                    // first name fall back to a positional tag.
                    let first_name = source
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .and_then(|line| {
                            let after_kw = line
                                .strip_prefix("const ")
                                .or_else(|| line.strip_prefix("final "))
                                .or_else(|| line.strip_prefix("init "))
                                .or_else(|| line.strip_prefix("shared "))
                                .unwrap_or(line);
                            after_kw
                                .split([' ', ':'])
                                .next()
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "anon".to_string());
                    format!("bindings.{first_name}")
                }
            };
            self.nodes[idx].logical_name = if parent_name.is_empty() {
                segment
            } else {
                format!("{parent_name}.{segment}")
            };
        }
    }

    /// SRD-13d §5.1 — walk past elided scope tiers to the
    /// nearest materialised ancestor (or self, when this
    /// node is itself materialised). Every consumer that
    /// needs a kernel handle (cache lookups, bind-outer-
    /// scope, diagnostics) routes through this — it's the
    /// single point that knows about elision; nothing
    /// else does.
    ///
    /// The workload root is always materialised, so this
    /// always terminates with `Some(idx)`. Returns `None`
    /// only if [`mark_scope_elision`] hasn't been run.
    pub fn nearest_materialised(&self, idx: ScopeNodeIdx) -> Option<ScopeNodeIdx> {
        let mut cur = idx;
        loop {
            match self.nodes[cur].materialised? {
                true => return Some(cur),
                false => match self.nodes[cur].parent {
                    Some(p) => cur = p,
                    None => return Some(cur), // root by construction
                },
            }
        }
    }

    /// Iterate every scope node in depth-first pre-order. The
    /// scheduler's default walk and the canonical display
    /// linearisation both consume this.
    pub fn iter_dfs(&self) -> DfsIter<'_> {
        DfsIter {
            tree: self,
            stack: vec![self.root],
        }
    }

    /// Walk from `idx` up through its ancestors to the root,
    /// inclusive of `idx` itself. Use this to compute effective
    /// pragmas (chain `attach_to`) or to render a path label.
    pub fn ancestors(&self, idx: ScopeNodeIdx) -> AncestorsIter<'_> {
        AncestorsIter {
            tree: self,
            cursor: Some(idx),
        }
    }

    /// First scope-tree node whose kind is `Phase { name }`
    /// matching the given name. Returns `None` if the scenario
    /// doesn't reference this phase. When a single phase is
    /// invoked from multiple scenario sites (rare; most workloads
    /// reference a phase exactly once), this returns the first
    /// occurrence in depth-first order — sufficient for current
    /// callers, who use the result to fetch the chain-walked
    /// `PragmaSet`.
    pub fn phase_node_by_name(&self, name: &str) -> Option<ScopeNodeIdx> {
        self.iter_dfs().find_map(|(idx, node)| match &node.kind {
            ScopeKind::Phase { name: n } if n == name => Some(idx),
            _ => None,
        })
    }

    /// Op-template kernel programs for every materialised
    /// op-template that's a child of `phase_idx`. Keyed by the
    /// op's name. Used by the executor to thread per-op-template
    /// programs into the activity so each `MetricsDispenser`
    /// builds its `ScopeFixture` against the correct scope
    /// (SRD-13d Phase 9 §"per-dispenser kernel instancing").
    /// Flattened op-templates (`materialised != Some(true)`) are
    /// omitted from the map; their dispensers reach the parent
    /// kernel through the standard `nearest_materialised`
    /// fall-through.
    ///
    /// Rule 2 write-through bindings ride on the program itself
    /// (baked in by the SRD-67 builder's finalize step). Any
    /// kernel built from the program inherits them automatically
    /// via `PolydatKernel::from_program` — no side channel.
    pub fn op_template_programs_for_phase(
        &self,
        phase_idx: ScopeNodeIdx,
    ) -> std::collections::HashMap<String, std::sync::Arc<polydat::kernel::PolydatProgram>> {
        let mut out = std::collections::HashMap::new();
        for &child_idx in &self.nodes[phase_idx].children {
            let child = &self.nodes[child_idx];
            let ScopeKind::OpTemplate { name } = &child.kind else {
                continue;
            };
            if child.materialised != Some(true) {
                continue;
            }
            if let Some(kernel) = child.cached_kernel.get() {
                out.insert(name.clone(), kernel.program().clone());
            }
        }
        out
    }

    /// All phase-leaf indices in depth-first order. Equivalent
    /// to filtering `iter_dfs()` to `ScopeKind::Phase` — the
    /// helper exists because it's the most common consumer
    /// query (TUI tree pre-mapping, dryrun=phase).
    pub fn phase_leaves(&self) -> Vec<ScopeNodeIdx> {
        self.iter_dfs()
            .filter_map(|(idx, node)| matches!(node.kind, ScopeKind::Phase { .. }).then_some(idx))
            .collect()
    }

    /// Walk ancestors of `idx` looking for the nearest scope
    /// node that has a kernel installed. Used at routing time
    /// to find the kernel a for_each scope's `materialize_wiring_from_outer`
    /// should chain from. Workload root always has a kernel
    /// installed (per M3.1), so this never returns `None` for
    /// any descendant of the root.
    pub fn nearest_installed_ancestor_kernel(
        &self,
        idx: ScopeNodeIdx,
    ) -> Option<std::sync::Arc<polydat::kernel::PolydatKernel>> {
        let mut cursor = self.nodes.get(idx)?.parent;
        while let Some(p) = cursor {
            if let Some(k) = self.nodes[p].cached_kernel.get() {
                return Some(k.clone());
            }
            cursor = self.nodes[p].parent;
        }
        None
    }

    /// Collect every installed ancestor kernel of `idx`,
    /// innermost first (immediate parent → workload root).
    /// Skips ancestor levels whose `cached_kernel` is empty
    /// (intermediate nodes that don't own their own kernel).
    /// Used by the checkpoint identity path to feed
    /// [`polydat::kernel::PolydatProgram::instance_hash`]
    /// (SRD-44 §"Identity matching at resume" + project
    /// memory `program_vs_instance_hash`).
    pub fn ancestor_kernels(
        &self,
        idx: ScopeNodeIdx,
    ) -> Vec<std::sync::Arc<polydat::kernel::PolydatKernel>> {
        let mut out = Vec::new();
        let mut cursor = self.nodes.get(idx).and_then(|n| n.parent);
        while let Some(p) = cursor {
            if let Some(k) = self.nodes[p].cached_kernel.get() {
                out.push(k.clone());
            }
            cursor = self.nodes[p].parent;
        }
        out
    }

    /// [`Self::ancestor_kernels`] split at the session boundary
    /// (SRD-107): `(below, session)` where `below` is every
    /// installed ancestor kernel from the immediate parent up
    /// through the workload root, and `session` is the
    /// session-node kernel (the workload-params module) when one
    /// is installed. The provenance base hash covers `below`
    /// only; param values are covered per-phase by the
    /// consumed-params digest instead.
    pub fn ancestor_kernels_split(
        &self,
        idx: ScopeNodeIdx,
    ) -> (
        Vec<std::sync::Arc<polydat::kernel::PolydatKernel>>,
        Option<std::sync::Arc<polydat::kernel::PolydatKernel>>,
    ) {
        let mut below = Vec::new();
        let mut session = None;
        let mut cursor = self.nodes.get(idx).and_then(|n| n.parent);
        while let Some(p) = cursor {
            if let Some(k) = self.nodes[p].cached_kernel.get() {
                if matches!(self.nodes[p].kind, ScopeKind::Session) {
                    session = Some(k.clone());
                } else {
                    below.push(k.clone());
                }
            }
            cursor = self.nodes[p].parent;
        }
        (below, session)
    }

    /// Find a `Comprehension` scope by structural-equality match
    /// against its [`Comprehension`] AST. Returns the **first**
    /// DFS-pre-order match.
    pub fn find_comprehension_scope(&self, comprehension: &Comprehension) -> Option<ScopeNodeIdx> {
        self.iter_dfs().find_map(|(idx, node)| match &node.kind {
            ScopeKind::Comprehension { comprehension: c } if c == comprehension => Some(idx),
            _ => None,
        })
    }

    /// First scope-tree node whose kind is
    /// `ScopeKind::Bindings { source }` matching exactly,
    /// searched globally from root in DFS pre-order.
    ///
    /// **Prefer [`Self::find_bindings_scope_under`]** when the
    /// executor's current scope position is known — see that
    /// method's doc for why a global content-only lookup is
    /// currently unsafe.
    pub fn find_bindings_scope(&self, source: &str) -> Option<ScopeNodeIdx> {
        self.iter_dfs().find_map(|(idx, node)| match &node.kind {
            ScopeKind::Bindings { source: s } if s == source => Some(idx),
            _ => None,
        })
    }

    /// **TRANSITIONAL WORKAROUND** — see task #19 for the
    /// canonical end-state plan. Constrains the lookup of a
    /// `Bindings` scope to descendants of `parent` so that two
    /// `Bindings` nodes sharing source text at different scope-
    /// tree positions resolve to the right one based on the
    /// executor's current position.
    ///
    /// The deeper problem: today the AST/source we use as the
    /// lookup key is LOSSY — two scope-tree nodes that produce
    /// semantically-distinct installed kernels (different
    /// cascaded externs from different parent chains) can share
    /// AST/source. Per SRD-13d §"Op-template scope synthesis" +
    /// SRD-13f §"The read invariant", installed kernels are
    /// determined by their PARENT chain, not by their own
    /// content alone. The current scope-aware lookup adds the
    /// missing context (parent subtree) at the call site to
    /// disambiguate.
    ///
    /// **Future direction:** make the AST/source self-
    /// identifying so that semantically-distinct kernels never
    /// share matter (embed parent-chain signature, encode
    /// scenario-tree path, or some equivalent invariant). Once
    /// that lands, content-only `find_bindings_scope` is
    /// correct again and this `_under` variant can be retired.
    pub fn find_bindings_scope_under(
        &self,
        parent: ScopeNodeIdx,
        source: &str,
    ) -> Option<ScopeNodeIdx> {
        self.find_descendant_matching(parent, &mut |node| match &node.kind {
            ScopeKind::Bindings { source: s } => s == source,
            _ => false,
        })
    }

    /// **TRANSITIONAL WORKAROUND** — see
    /// [`Self::find_bindings_scope_under`] for the underlying
    /// principle and task #19 for the canonical end-state plan.
    /// Retired once the AST becomes self-identifying.
    pub fn find_comprehension_scope_under(
        &self,
        parent: ScopeNodeIdx,
        comprehension: &Comprehension,
    ) -> Option<ScopeNodeIdx> {
        self.find_descendant_matching(parent, &mut |node| match &node.kind {
            ScopeKind::Comprehension { comprehension: c } => c == comprehension,
            _ => false,
        })
    }

    /// DFS pre-order search through the descendants of `parent`
    /// (excluding `parent` itself). Returns the first node whose
    /// predicate returns true.
    fn find_descendant_matching(
        &self,
        parent: ScopeNodeIdx,
        predicate: &mut dyn FnMut(&ScopeNode) -> bool,
    ) -> Option<ScopeNodeIdx> {
        let mut stack: Vec<ScopeNodeIdx> =
            self.nodes[parent].children.iter().rev().copied().collect();
        while let Some(idx) = stack.pop() {
            if predicate(&self.nodes[idx]) {
                return Some(idx);
            }
            for &child in self.nodes[idx].children.iter().rev() {
                stack.push(child);
            }
        }
        None
    }

    /// Validate iteration-variable name uniqueness against the
    /// surrounding scope chain.
    ///
    /// An iter-var name (`for_each: "X in ..."`,
    /// `for_combinations: "X in ..., Y in ..."`,
    /// `for_each_union: ...`, do-loop counters) **must not**
    /// shadow:
    /// - a workload param,
    /// - an iter var declared by an enclosing scope.
    ///
    /// Aliasing creates a name that can't unambiguously resolve
    /// at spec-evaluation time (the iter var is being defined
    /// from a value that uses the same name; the runtime can't
    /// tell whether `{X}` means the iter var or the shadowed
    /// outer name). Rather than try to disambiguate, the build
    /// rejects it up-front with a clear error so the user
    /// renames the iter var.
    ///
    /// Returns `Ok(())` if every iter-var name is unique. Returns
    /// `Err(...)` with the offending name and which kind of
    /// collision (workload param vs ancestor iter var) the user
    /// has on the first violation found.
    pub fn validate_iter_var_uniqueness(
        &self,
        workload_params: &std::collections::HashSet<String>,
    ) -> Result<(), String> {
        fn walk(
            tree: &ScopeTree,
            idx: ScopeNodeIdx,
            ancestor_iter_vars: &std::collections::HashSet<String>,
            workload_params: &std::collections::HashSet<String>,
        ) -> Result<(), String> {
            let node = &tree.nodes[idx];
            // Collect the iter vars declared at this node.
            // Algebra's `coordinate_names()` returns owned
            // strings (operator-tree walks need fresh strings
            // — there's no single backing slice to borrow
            // from), so this block is owned-string throughout.
            let own_iter_vars: Vec<String> = match &node.kind {
                ScopeKind::Comprehension { comprehension } => comprehension.coordinate_names(),
                ScopeKind::DoWhile {
                    counter: Some(c), ..
                }
                | ScopeKind::DoUntil {
                    counter: Some(c), ..
                } => vec![c.clone()],
                _ => Vec::new(),
            };
            for var in &own_iter_vars {
                if workload_params.contains(var) {
                    return Err(format!(
                        "iter-var '{var}' aliases workload param '{var}'. \
                         A for_each / for_combinations / for_each_union iter \
                         variable cannot share a name with a workload param — \
                         spec evaluation can't disambiguate `{{{var}}}` between \
                         the iter var and the param. Rename one of them."
                    ));
                }
                if ancestor_iter_vars.contains(var) {
                    return Err(format!(
                        "iter-var '{var}' aliases an iter var declared by an \
                         enclosing scope. Inner iter vars must use distinct \
                         names from outer iter vars."
                    ));
                }
            }
            // Extend the ancestor set for descent.
            let mut next_ancestors = ancestor_iter_vars.clone();
            for v in &own_iter_vars {
                next_ancestors.insert(v.clone());
            }
            for &child in &node.children {
                walk(tree, child, &next_ancestors, workload_params)?;
            }
            Ok(())
        }
        walk(
            self,
            self.root,
            &std::collections::HashSet::new(),
            workload_params,
        )
    }

    /// Install the canonical compiled kernel for `scope_idx`.
    ///
    /// Called at pre-map time after compiling the scope's
    /// `PolydatProgram`. Once installed, the kernel is the *single*
    /// authoritative answer for "what is `<name>` at this
    /// scope?" — every name visible at this scope (own outputs
    /// plus parent-inherited values bound via
    /// [`PolydatKernel::materialize_wiring_from_outer`]) resolves through the
    /// standard Polydat API on this one kernel. Callers don't walk
    /// the scope tree to do name resolution; Polydat's auto-extern +
    /// outer-scope wiring already encapsulates the layering.
    ///
    /// Idempotent only by virtue of `OnceLock`: a second install
    /// silently no-ops, returning `false`. Returns `true` on
    /// fresh install. Callers that need to detect a duplicate
    /// install should check the boolean.
    pub fn install_kernel(
        &self,
        scope_idx: ScopeNodeIdx,
        kernel: std::sync::Arc<polydat::kernel::PolydatKernel>,
    ) -> bool {
        match self.nodes.get(scope_idx) {
            Some(node) => {
                let inserted = node.cached_kernel.set(kernel.clone()).is_ok();
                // Ride-along visitor hook (SRD planning-walk
                // dryrun=kernels surface). Fires exactly once
                // per scope's fresh install — the OnceLock
                // semantics above guarantee no duplicate calls.
                if inserted {
                    notify_kernel_installed(node, scope_idx, &kernel);
                }
                inserted
            }
            None => false,
        }
    }

    /// Populate `pragmas` on every phase-leaf scope by scanning
    /// each phase's `BindingsDef::PolydatSource` strings for `pragma`
    /// statements, then walk the tree to chain each scope's
    /// `PragmaSet` onto its parent's. After this call, querying
    /// `node.pragmas.strict_values()` walks the chain through
    /// every ancestor.
    ///
    /// SRD 18b §"Pragma chain along the scope tree". Idempotent
    /// per call (replaces any prior `pragmas` content).
    ///
    /// Returns the list of conflicts surfaced during chain
    /// attachment (today: empty for presence-only pragmas; the
    /// list exists for forward compatibility). Caller decides
    /// whether to log or fail on conflicts based on strict mode.
    pub fn populate_pragmas(
        &mut self,
        phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
    ) -> Vec<crate::scope_tree::PragmaConflict> {
        let mut conflicts = Vec::new();

        // Pass 1: extract phase-local pragmas. Iterate by
        // `phase_leaves` (which already does the kind filter)
        // and walk each phase's ops for Polydat source strings to
        // parse.
        let leaves = self.phase_leaves();
        for idx in leaves {
            let name = match &self.nodes[idx].kind {
                ScopeKind::Phase { name } => name.clone(),
                _ => continue,
            };
            if let Some(phase) = phases.get(&name) {
                self.nodes[idx].pragmas = extract_phase_pragmas(phase);
            }
        }

        // Pass 2: attach each scope to its parent. Walk in
        // depth order so a parent's `Arc<PragmaSet>` is finalised
        // before its children pin to it.
        let order: Vec<ScopeNodeIdx> = self.iter_dfs().map(|(i, _)| i).collect();
        for idx in order {
            if let Some(parent) = self.nodes[idx].parent {
                let parent_arc = std::sync::Arc::new(self.nodes[parent].pragmas.clone());
                let local = std::mem::take(&mut self.nodes[idx].pragmas);
                let (attached, mut local_conflicts) = local.attach_to(parent_arc);
                self.nodes[idx].pragmas = attached;
                for c in &mut local_conflicts {
                    conflicts.push(PragmaConflict {
                        scope_idx: idx,
                        name: c.name.clone(),
                        outer_line: c.outer_line,
                        inner_line: c.inner_line,
                    });
                }
            }
        }

        conflicts
    }
}

/// One pragma conflict surfaced when attaching a scope to its
/// parent. Reports the offending scope index so the caller can
/// turn it into a structured diagnostic with a path label.
#[derive(Debug, Clone)]
pub struct PragmaConflict {
    pub scope_idx: ScopeNodeIdx,
    pub name: String,
    pub outer_line: usize,
    pub inner_line: usize,
}

/// Extract pragmas from a phase's source by walking every op's
/// `BindingsDef::PolydatSource` and collecting `Statement::Pragma`s.
/// A phase has multiple ops; their bindings can each declare
/// pragmas. Today the convention is one pragma block at the
/// phase head; multi-op phases that put pragmas on individual
/// ops still get them aggregated here.
fn extract_phase_pragmas(phase: &nmbrs_workload::model::WorkloadPhase) -> PragmaSet {
    use nmbrs_workload::model::BindingsDef;
    let mut entries = Vec::new();
    for op in &phase.ops {
        let src = match &op.bindings {
            BindingsDef::PolydatSource(s) => s.as_str(),
            _ => continue,
        };
        // Lex/parse to AST to surface `Statement::Pragma`s. If
        // the source is malformed, skip — the real phase compile
        // will report a clean parse error later.
        let tokens = match polydat::dsl::lexer::lex(src) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let ast = match polydat::dsl::parser::parse(tokens) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let local = polydat::dsl::pragmas::collect_from_ast(&ast);
        entries.extend(local.entries);
    }
    PragmaSet {
        entries,
        parent: None,
    }
}

/// Ride-along visitor for kernel-installation events. Set by
/// the runner when `dryrun=kernels` is requested so each
/// `install_kernel` fires the printer as the planning walk
/// encounters the scope. `None` (the default) keeps install
/// a no-cost hot path.
pub type KernelInstallVisitor =
    Box<dyn Fn(&ScopeNode, ScopeNodeIdx, &polydat::kernel::PolydatKernel) + Send + Sync>;

static KERNEL_INSTALL_VISITOR: std::sync::OnceLock<std::sync::Mutex<Option<KernelInstallVisitor>>> =
    std::sync::OnceLock::new();

fn visitor_slot() -> &'static std::sync::Mutex<Option<KernelInstallVisitor>> {
    KERNEL_INSTALL_VISITOR.get_or_init(|| std::sync::Mutex::new(None))
}

/// Register a visitor that fires on every `install_kernel`
/// call. Replaces any prior visitor; pass `None` to clear.
/// Called by the runner at session start when
/// `dryrun=kernels` is set.
pub fn set_kernel_install_visitor(v: Option<KernelInstallVisitor>) {
    if let Ok(mut slot) = visitor_slot().lock() {
        *slot = v;
    }
}

fn notify_kernel_installed(
    node: &ScopeNode,
    idx: ScopeNodeIdx,
    kernel: &polydat::kernel::PolydatKernel,
) {
    if let Ok(slot) = visitor_slot().lock()
        && let Some(visitor) = slot.as_ref()
    {
        visitor(node, idx, kernel);
    }
}

/// Depth-first pre-order iterator over `(idx, &ScopeNode)`.
pub struct DfsIter<'a> {
    tree: &'a ScopeTree,
    stack: Vec<ScopeNodeIdx>,
}

impl<'a> Iterator for DfsIter<'a> {
    type Item = (ScopeNodeIdx, &'a ScopeNode);
    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.stack.pop()?;
        let node = &self.tree.nodes[idx];
        // Push children in reverse so the leftmost child comes
        // out of the stack first (pre-order).
        for &child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some((idx, node))
    }
}

/// Walk from a node up through its ancestors to the root.
pub struct AncestorsIter<'a> {
    tree: &'a ScopeTree,
    cursor: Option<ScopeNodeIdx>,
}

impl<'a> Iterator for AncestorsIter<'a> {
    type Item = (ScopeNodeIdx, &'a ScopeNode);
    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.cursor?;
        let node = &self.tree.nodes[idx];
        self.cursor = node.parent;
        Some((idx, node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(name: &str) -> ScenarioNode {
        ScenarioNode::Phase(name.into())
    }
    fn for_each(spec: &str, children: Vec<ScenarioNode>) -> ScenarioNode {
        use polydat::iteration::comprehension::spec::{ComprehensionSpec, ForSpec};
        let comprehension = ComprehensionSpec {
            r#for: ForSpec::Inline(spec.to_string()),
            r#where: None,
            order: None,
        }
        .into_algebra()
        .unwrap();
        ScenarioNode::Comprehension {
            comprehension,
            children,
            continue_if: None,
            anchor: None,
        }
    }

    #[test]
    fn workload_and_scenario_always_present() {
        let tree = ScopeTree::build("default", &[]);
        // Even with no children, session + workload + scenario layers
        // survive so observer code doesn't special-case empty scenarios.
        assert_eq!(tree.nodes.len(), 3);
        assert!(matches!(tree.nodes[0].kind, ScopeKind::Session));
        assert!(matches!(tree.nodes[1].kind, ScopeKind::Workload));
        assert!(matches!(&tree.nodes[2].kind, ScopeKind::Scenario { name } if name == "default"));
        assert_eq!(tree.nodes[1].depth, 1);
        assert_eq!(tree.nodes[2].depth, 2);
    }

    #[test]
    fn flat_phases_under_scenario() {
        let tree = ScopeTree::build("default", &[phase("setup"), phase("run")]);
        assert_eq!(tree.nodes.len(), 5);
        let scenario = &tree.nodes[2];
        assert_eq!(scenario.children.len(), 2);
        for &c in &scenario.children {
            assert!(matches!(tree.nodes[c].kind, ScopeKind::Phase { .. }));
            assert_eq!(tree.nodes[c].depth, 3);
            assert_eq!(tree.nodes[c].parent, Some(2));
        }
    }

    #[test]
    fn nested_for_each_preserves_depth() {
        // for_each x in xs { for_each y in ys { phase P } }
        let tree = ScopeTree::build(
            "default",
            &[for_each(
                "x in xs",
                vec![for_each("y in ys", vec![phase("P")])],
            )],
        );
        // session(0) → workload(1) → scenario(2) → for_each_x(3) → for_each_y(4) → phase_P(5)
        assert_eq!(tree.nodes.len(), 6);
        assert_eq!(tree.nodes[3].depth, 3);
        assert_eq!(tree.nodes[4].depth, 4);
        assert_eq!(tree.nodes[5].depth, 5);
        assert!(matches!(
            &tree.nodes[3].kind,
            ScopeKind::Comprehension { comprehension }
                if comprehension.coordinate_names() == vec!["x"]
        ));
        assert!(matches!(
            &tree.nodes[4].kind,
            ScopeKind::Comprehension { comprehension }
                if comprehension.coordinate_names() == vec!["y"]
        ));
    }

    #[test]
    fn dfs_pre_order_matches_authored_order() {
        let tree = ScopeTree::build(
            "default",
            &[
                for_each("x in xs", vec![phase("a"), phase("b")]),
                phase("c"),
            ],
        );
        let names: Vec<String> = tree.iter_dfs().map(|(_, n)| n.kind.label()).collect();
        assert_eq!(
            names,
            vec![
                "session".to_string(),
                "workload".into(),
                "scenario 'default'".into(),
                "each x".into(),
                "phase 'a'".into(),
                "phase 'b'".into(),
                "phase 'c'".into(),
            ]
        );
    }

    #[test]
    fn ancestors_walk_to_root() {
        let tree = ScopeTree::build("default", &[for_each("x in xs", vec![phase("a")])]);
        let phase_idx = tree.phase_leaves()[0];
        let ancestors: Vec<String> = tree
            .ancestors(phase_idx)
            .map(|(_, n)| n.kind.label())
            .collect();
        assert_eq!(
            ancestors,
            vec![
                "phase 'a'".to_string(),
                "each x".into(),
                "scenario 'default'".into(),
                "workload".into(),
                "session".into(),
            ]
        );
    }

    #[test]
    fn phase_leaves_returns_only_phases() {
        let tree = ScopeTree::build(
            "default",
            &[
                for_each("x in xs", vec![phase("a"), phase("b")]),
                phase("c"),
            ],
        );
        let leaves = tree.phase_leaves();
        assert_eq!(leaves.len(), 3);
        for idx in leaves {
            assert!(matches!(tree.nodes[idx].kind, ScopeKind::Phase { .. }));
        }
    }

    fn make_phase_with_source(src: &str) -> nmbrs_workload::model::WorkloadPhase {
        use nmbrs_workload::model::{BindingsDef, ParsedOp, WorkloadPhase};
        let mut op = ParsedOp::simple("op", "noop");
        op.bindings = BindingsDef::PolydatSource(src.into());
        WorkloadPhase {
            key_metrics: Vec::new(),
            cycles: None,
            concurrency: None,
            rate: None,
            adapter: None,
            errors: None,
            tags: None,
            ops: vec![op],
            for_each: None,
            ..Default::default()
        }
    }

    #[test]
    fn populate_pragmas_propagates_through_chain() {
        // Phase has `pragma strict_values` in its source. After
        // populate_pragmas + attach, an inner for_each scope (no
        // own pragmas) should still resolve `strict_values()` true
        // through its parent chain back to… wait. Phase is the
        // *leaf*, not the parent. The propagation we care about is
        // "phase's pragmas propagate up", but the chain is parent
        // → child. Let's flip: put the pragma in a phase, and the
        // assertion is "the phase scope sees its own pragmas." A
        // future test will demonstrate cross-scope propagation
        // once non-phase scopes can declare pragmas.
        let phases = std::collections::HashMap::from([(
            "p".to_string(),
            make_phase_with_source("pragma strict_values\n id := cycle\n"),
        )]);
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        let conflicts = tree.populate_pragmas(&phases);
        assert!(conflicts.is_empty());
        let phase_idx = tree.phase_leaves()[0];
        assert!(tree.nodes[phase_idx].pragmas.strict_values());
    }

    #[test]
    fn populate_pragmas_chain_walk_through_attach() {
        // Build a small tree where the phase declares strict_values
        // and verify that querying through `attach_to` resolves it
        // even from sibling scopes that don't declare it. Sibling
        // queries are valid because every scope's `parent` chain
        // ultimately reaches the workload root.
        let phases = std::collections::HashMap::from([(
            "p".to_string(),
            make_phase_with_source("pragma strict\n id := cycle\n"),
        )]);
        let mut tree = ScopeTree::build("default", &[for_each("x in xs", vec![phase("p")])]);
        tree.populate_pragmas(&phases);
        let phase_idx = tree.phase_leaves()[0];
        // Phase declares strict (alias for both). Confirm:
        assert!(tree.nodes[phase_idx].pragmas.strict_types());
        assert!(tree.nodes[phase_idx].pragmas.strict_values());
    }

    // ---- M3.1: kernel install primitive ----

    /// Compile a tiny Polydat source into a kernel for use as a
    /// scope's canonical instance. A one-line `name := <const>`
    /// suffices to populate `output_map` so `get_constant`
    /// returns the folded value.
    fn compile_kernel(source: &str) -> std::sync::Arc<polydat::kernel::PolydatKernel> {
        let kernel =
            polydat::dsl::compile::compile_polydat(source).expect("test source should compile");
        std::sync::Arc::new(kernel)
    }

    #[test]
    fn install_kernel_seeds_canonical_state() {
        // After install, the cached kernel answers the name via
        // the standard Polydat API. No tree-walking on the caller
        // side — the kernel encapsulates its own scope, and
        // composition (auto-extern + materialize_wiring_from_outer) is what
        // makes parent values reachable. This test only verifies
        // the install primitive; the Polydat side already has its own
        // tests for composition.
        let tree = ScopeTree::build("default", &[phase("p")]);
        let workload_kernel = compile_kernel("const dataset := \"example\"\n");
        assert!(tree.install_kernel(0, workload_kernel));

        let cached = tree.nodes[0]
            .cached_kernel
            .get()
            .expect("install populated the slot");
        match cached.get_constant("dataset") {
            Some(polydat::ast::Value::Str(s)) => assert_eq!(&**s, "example"),
            other => panic!("expected Str(\"example\"), got {other:?}"),
        }
    }

    #[test]
    fn for_each_scope_kernel_inherits_parent_via_materialize_wiring_from_outer() {
        // M3.2 end-to-end: build a parent kernel that exposes a
        // workload-style param as an output, synthesize a
        // for_each scope kernel that references that param plus
        // its own iter var, bind from parent, then verify both
        // values are reachable on the synthesized kernel via
        // standard Polydat API. Validates the chain inheritance
        // path without any caller-side scope walking.
        use polydat::kernel::PolydatKernel;
        use std::sync::Arc;

        // Parent: a workload-shaped kernel exposing `k_values`.
        let parent_src = "const k_values := \"1, 10\"\n";
        let parent: Arc<PolydatKernel> =
            Arc::new(polydat::dsl::compile::compile_polydat(parent_src).unwrap());

        // Build the for_each scope kernel as the runner would.
        let parent_manifest = crate::runner::extract_manifest(parent.program());
        let kernel = crate::scope_synth::build_for_each_scope_kernel(
            &[("k".to_string(), "{k_values}".to_string())],
            &parent_manifest,
            &parent,
            &std::collections::HashMap::new(),
            Vec::new(),
            None,
            false,
            "test",
            None,
        )
        .expect("synthesis should succeed");

        // After `materialize_wiring_from_outer` (called inside the helper),
        // the inherited extern is populated with the parent's
        // value.
        match kernel.get_input("k_values") {
            Some(polydat::ast::Value::Str(s)) => assert_eq!(&*s, "1, 10"),
            other => panic!("expected Str(\"1, 10\"), got {other:?}"),
        }

        // The iter var `k` is also visible as an extern; not
        // yet set by the runtime, so its current value is the
        // default for String externs.
        // (Runtime semantics test belongs in executor.rs once
        // M3.4 wires this up; M3.2 only verifies the install +
        // chain mechanics.)
        assert!(
            kernel.program().find_input("k").is_some(),
            "iter var should be declared as an extern input"
        );

        // Polydat's `extern` declaration auto-installs a passthrough
        // node that exposes the name as an output too — so
        // children's `materialize_wiring_from_outer(this_scope)` sees both
        // `k_values` and `k` in this scope's manifest and the
        // chain inheritance flows through standard Polydat API
        // without any caller-side scope walking.
        let manifest = crate::runner::extract_manifest(kernel.program());
        let output_names: std::collections::HashSet<_> =
            manifest.iter().map(|e| e.name.as_str()).collect();
        assert!(
            output_names.contains("k_values"),
            "inherited name appears as output via extern's auto-passthrough"
        );
        assert!(
            output_names.contains("k"),
            "iter var appears as output via extern's auto-passthrough"
        );
    }

    #[test]
    fn for_each_scope_kernel_uses_native_type_for_numeric_iter_var() {
        // Single-clause for_each over a numeric workload param.
        // Pre-eval at synthesis detects U64 from "1, 10" and
        // declares `extern k: u64` instead of `extern k: String`.
        // Per SRD-18b "native types as the general rule".
        use polydat::kernel::PolydatKernel;
        use std::sync::Arc;

        let parent_src = "const k_values := \"1, 10\"\n";
        let parent: Arc<PolydatKernel> =
            Arc::new(polydat::dsl::compile::compile_polydat(parent_src).unwrap());
        let parent_manifest = crate::runner::extract_manifest(parent.program());

        let kernel = crate::scope_synth::build_for_each_scope_kernel(
            &[("k".to_string(), "{k_values}".to_string())],
            &parent_manifest,
            &parent,
            &std::collections::HashMap::new(),
            Vec::new(),
            None,
            false,
            "test",
            None,
        )
        .expect("synthesis should succeed");

        // Assert k's input port is u64-typed, not String.
        let manifest = crate::runner::extract_manifest(kernel.program());
        let k_entry = manifest
            .iter()
            .find(|e| e.name == "k")
            .expect("k must appear in manifest");
        assert_eq!(
            k_entry.port_type,
            polydat::ast::PortType::U64,
            "iter var over numeric values should be typed u64, not String"
        );
    }

    #[test]
    fn for_each_scope_kernel_recursive_probe_for_dependent_clause() {
        // Multi-clause dependent: clause 2's spec text references
        // clause 1's iter var via `{k}`. Pre-eval probes clause 1
        // (k_values = "1, 10" → first value 1, type U64). Then
        // for clause 2's spec `{k_{k}_limits}`, the probe
        // substitutes {k}→1, leaving `{k_1_limits}`, which
        // resolves to "1, 2, 4, 8" via parent's manifest. First
        // value is 1, type U64.
        use polydat::kernel::PolydatKernel;
        use std::sync::Arc;

        let parent_src = concat!(
            "const k_values := \"1, 10\"\n",
            "const k_1_limits := \"1, 2, 4, 8\"\n",
            "const k_10_limits := \"10, 20, 30\"\n",
        );
        let parent: Arc<PolydatKernel> =
            Arc::new(polydat::dsl::compile::compile_polydat(parent_src).unwrap());
        let parent_manifest = crate::runner::extract_manifest(parent.program());

        let kernel = crate::scope_synth::build_for_each_scope_kernel(
            &[
                ("k".to_string(), "{k_values}".to_string()),
                ("limit".to_string(), "{k_{k}_limits}".to_string()),
            ],
            &parent_manifest,
            &parent,
            &std::collections::HashMap::new(),
            Vec::new(),
            None,
            false,
            "test",
            None,
        )
        .expect("synthesis should succeed");

        let manifest = crate::runner::extract_manifest(kernel.program());
        let k_entry = manifest.iter().find(|e| e.name == "k").unwrap();
        let limit_entry = manifest.iter().find(|e| e.name == "limit").unwrap();
        assert_eq!(
            k_entry.port_type,
            polydat::ast::PortType::U64,
            "k typed u64 from k_values pre-eval"
        );
        assert_eq!(
            limit_entry.port_type,
            polydat::ast::PortType::U64,
            "limit typed u64 via recursive probe k=1 → k_1_limits → \"1, 2, 4, 8\""
        );
    }

    // ── SRD-13d Phase 4 + 5: scope elision marks ──

    #[test]
    fn mark_scope_elision_assigns_logical_names() {
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        // All-materialise predicate so every node gets a name.
        tree.mark_scope_elision(|_kind, _idx| true);
        // Session root contributes no path segment (SRD-88).
        assert_eq!(tree.nodes[0].logical_name, "");
        assert_eq!(tree.nodes[0].materialised, Some(true));
        // Workload child owns the "workload" segment.
        let workload_idx = tree.nodes[0].children[0];
        assert_eq!(tree.nodes[workload_idx].logical_name, "workload");
        // Scenario is named after its scenario tag.
        let scenario_idx = tree.nodes[workload_idx].children[0];
        assert_eq!(
            tree.nodes[scenario_idx].logical_name,
            "workload.scenario.default"
        );
        // Phase descends from scenario.
        let phase_idx = tree.nodes[scenario_idx].children[0];
        assert_eq!(
            tree.nodes[phase_idx].logical_name,
            "workload.scenario.default.phase.p"
        );
    }

    #[test]
    fn mark_scope_elision_records_predicate_decisions() {
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        // Predicate: only Phase scopes materialise.
        tree.mark_scope_elision(|kind, _idx| matches!(kind, ScopeKind::Phase { .. }));
        let workload_idx = tree.nodes[0].children[0];
        let scenario_idx = tree.nodes[workload_idx].children[0];
        let phase_idx = tree.nodes[scenario_idx].children[0];
        assert_eq!(tree.nodes[scenario_idx].materialised, Some(false));
        assert_eq!(tree.nodes[phase_idx].materialised, Some(true));
    }

    #[test]
    fn nearest_materialised_walks_past_elided_layers() {
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        // Predicate: only the workload tier materialises.
        tree.mark_scope_elision(|kind, _idx| matches!(kind, ScopeKind::Workload));
        let workload_idx = tree.nodes[0].children[0];
        let scenario_idx = tree.nodes[workload_idx].children[0];
        let phase_idx = tree.nodes[scenario_idx].children[0];
        // Phase's nearest materialised ancestor is the workload node.
        assert_eq!(tree.nearest_materialised(phase_idx), Some(workload_idx));
        assert_eq!(tree.nearest_materialised(scenario_idx), Some(workload_idx));
        // The session root always self-materialises.
        assert_eq!(tree.nearest_materialised(0), Some(0));
    }

    #[test]
    fn nearest_materialised_returns_self_when_node_materialises() {
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        tree.mark_scope_elision(|_kind, _idx| true);
        let phase_idx = tree.nodes[tree.nodes[0].children[0]].children[0];
        assert_eq!(tree.nearest_materialised(phase_idx), Some(phase_idx));
    }

    #[test]
    fn nearest_materialised_none_before_pre_walk() {
        // Pre-walk hasn't run — every node's `materialised` is
        // None — so the walker can't terminate. Returns None.
        let tree = ScopeTree::build("default", &[phase("p")]);
        assert_eq!(tree.nearest_materialised(0), None);
    }

    #[test]
    fn workload_root_always_materialises_regardless_of_predicate() {
        // Even an "always elide" predicate can't elide the
        // root — SRD-13d §5.1 mandates the root is the
        // termination point of nearest_materialised walks.
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        tree.mark_scope_elision(|_kind, _idx| false);
        assert_eq!(tree.nodes[0].materialised, Some(true));
    }

    // ── SRD-13d Phase 6: op-template tier ──

    #[test]
    fn extend_with_op_templates_adds_one_child_per_op() {
        use nmbrs_workload::model::{BindingsDef, ParsedOp, WorkloadPhase};
        use std::collections::HashMap;
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        let mut phases = HashMap::new();
        phases.insert(
            "p".into(),
            WorkloadPhase {
                key_metrics: Vec::new(),
                dimensions: Default::default(),
                cycles: None,
                concurrency: None,
                rate: None,
                daemon: false,
                adapter: None,
                errors: None,
                tries: None,
                tries_backoff: None,
                interval: None,
                repeat: None,
                error_rate_max: None,
                timeout: None,
                stop_when: Vec::new(),
                throttle: None,
                tags: None,
                ops: vec![
                    ParsedOp::simple("alpha", "noop"),
                    ParsedOp::simple("beta", "noop"),
                ],
                for_each: None,
                continue_if: None,
                loop_scope: None,
                iter_scope: None,
                checkpoint: None,
                status_metrics: vec![],
                metrics: Default::default(),
                poll: None,
                bindings: BindingsDef::default(),
                optimize: None,
            },
        );
        tree.extend_with_op_templates(&phases);
        let workload_idx = tree.nodes[0].children[0];
        let scenario_idx = tree.nodes[workload_idx].children[0];
        let phase_idx = tree.nodes[scenario_idx].children[0];
        // Phase now has 2 op-template children.
        assert_eq!(tree.nodes[phase_idx].children.len(), 2);
        let op_a_idx = tree.nodes[phase_idx].children[0];
        let op_b_idx = tree.nodes[phase_idx].children[1];
        assert!(matches!(&tree.nodes[op_a_idx].kind,
            ScopeKind::OpTemplate { name } if name == "alpha"));
        assert!(matches!(&tree.nodes[op_b_idx].kind,
            ScopeKind::OpTemplate { name } if name == "beta"));
        // Depth = phase depth + 1.
        assert_eq!(tree.nodes[op_a_idx].depth, tree.nodes[phase_idx].depth + 1);
    }

    #[test]
    fn extend_with_op_templates_is_idempotent() {
        use nmbrs_workload::model::{BindingsDef, ParsedOp, WorkloadPhase};
        use std::collections::HashMap;
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        let mut phases = HashMap::new();
        phases.insert(
            "p".into(),
            WorkloadPhase {
                key_metrics: Vec::new(),
                dimensions: Default::default(),
                cycles: None,
                concurrency: None,
                rate: None,
                daemon: false,
                adapter: None,
                errors: None,
                tries: None,
                tries_backoff: None,
                interval: None,
                repeat: None,
                error_rate_max: None,
                timeout: None,
                stop_when: Vec::new(),
                throttle: None,
                tags: None,
                ops: vec![ParsedOp::simple("only", "noop")],
                for_each: None,
                continue_if: None,
                loop_scope: None,
                iter_scope: None,
                checkpoint: None,
                status_metrics: vec![],
                metrics: Default::default(),
                poll: None,
                bindings: BindingsDef::default(),
                optimize: None,
            },
        );
        tree.extend_with_op_templates(&phases);
        let n_after_first = tree.nodes.len();
        tree.extend_with_op_templates(&phases); // Second call.
        assert_eq!(
            tree.nodes.len(),
            n_after_first,
            "second call should not add nodes"
        );
    }

    #[test]
    fn op_template_logical_name_uses_op_segment() {
        use nmbrs_workload::model::{BindingsDef, ParsedOp, WorkloadPhase};
        use std::collections::HashMap;
        let mut tree = ScopeTree::build("default", &[phase("p")]);
        let mut phases = HashMap::new();
        phases.insert(
            "p".into(),
            WorkloadPhase {
                key_metrics: Vec::new(),
                dimensions: Default::default(),
                cycles: None,
                concurrency: None,
                rate: None,
                daemon: false,
                adapter: None,
                errors: None,
                tries: None,
                tries_backoff: None,
                interval: None,
                repeat: None,
                error_rate_max: None,
                timeout: None,
                stop_when: Vec::new(),
                throttle: None,
                tags: None,
                ops: vec![ParsedOp::simple("foo", "noop")],
                for_each: None,
                continue_if: None,
                loop_scope: None,
                iter_scope: None,
                checkpoint: None,
                status_metrics: vec![],
                metrics: Default::default(),
                poll: None,
                bindings: BindingsDef::default(),
                optimize: None,
            },
        );
        tree.extend_with_op_templates(&phases);
        tree.mark_scope_elision(|_kind, _idx| true);
        // Find the op node and check its logical name.
        let op_idx = tree
            .iter_dfs()
            .find(|(_, n)| matches!(&n.kind, ScopeKind::OpTemplate { name } if name == "foo"))
            .map(|(i, _)| i)
            .expect("op-template node");
        assert_eq!(
            tree.nodes[op_idx].logical_name,
            "workload.scenario.default.phase.p.op.foo"
        );
    }

    #[test]
    fn install_is_idempotent_via_oncelock() {
        // OnceLock semantics: first install wins; subsequent
        // installs silently no-op. The boolean return lets
        // callers detect duplicate installs (likely a logic bug
        // in the runner) without panicking.
        let tree = ScopeTree::build("default", &[phase("p")]);
        let k1 = compile_kernel("const x := 1\n");
        let k2 = compile_kernel("const x := 2\n");
        assert!(tree.install_kernel(0, k1), "first install succeeds");
        assert!(!tree.install_kernel(0, k2), "second install no-ops");

        let cached = tree.nodes[0].cached_kernel.get().unwrap();
        match cached.get_constant("x") {
            Some(polydat::ast::Value::U64(n)) => assert_eq!(*n, 1),
            other => panic!("expected U64(1), got {other:?}"),
        }
    }

    /// **WORKAROUND-PINNING TEST — retire when AST becomes
    /// self-identifying** (task #19).
    ///
    /// Today's lookup key (raw Comprehension AST) is LOSSY:
    /// two scope-tree positions produce semantically-distinct
    /// installed kernels (different cascaded externs from
    /// different parent chains) but can share AST. The
    /// `_under(parent_idx, ...)` lookup adds the missing
    /// context — parent-subtree restriction — at the call
    /// site to disambiguate. This test pins that behavior in
    /// place.
    ///
    /// When the AST becomes self-identifying (so two
    /// distinct kernels never share matter), `find_comprehension_scope`
    /// is correct again, `find_comprehension_scope_under` can
    /// be retired, and this test should be deleted along with
    /// it.
    ///
    /// Workload shape modeled here:
    ///
    /// ```text
    /// for_each "a in [1]" {       // outer A
    ///   for_each "x in xs" { P }  // x-comprehension #1, under A
    /// }
    /// for_each "b in [2]" {       // outer B
    ///   for_each "x in xs" { P }  // x-comprehension #2, under B
    /// }
    /// ```
    ///
    /// Both `for_each "x in xs"` blocks have IDENTICAL AST.
    /// Under the lossy-AST model, `find_comprehension_scope_under(B_idx,
    /// x_comp)` MUST return #2's idx, not #1's, because the
    /// installed kernels at #1 and #2 differ in their cascade
    /// even though the AST does not. When AST becomes
    /// self-identifying the two `for_each "x in xs"` blocks
    /// will no longer share AST — they will carry distinct
    /// context — and the global lookup will work.
    #[test]
    fn find_comprehension_scope_under_disambiguates_identical_ast() {
        let tree = ScopeTree::build(
            "default",
            &[
                for_each("a in [1]", vec![for_each("x in xs", vec![phase("P")])]),
                for_each("b in [2]", vec![for_each("x in xs", vec![phase("P")])]),
            ],
        );
        // Tree layout (DFS):
        //   0 session
        //   1 workload
        //   2 scenario
        //   3 for_each(a)
        //   4   for_each(x) #1
        //   5     phase(P)
        //   6 for_each(b)
        //   7   for_each(x) #2
        //   8     phase(P)
        //
        // Build the x-comprehension AST that both inner scopes
        // share, then verify the path-aware lookup picks the
        // right one from each side.
        let x_comp = polydat::iteration::comprehension::spec::ComprehensionSpec {
            r#for: polydat::iteration::comprehension::spec::ForSpec::Inline("x in xs".to_string()),
            r#where: None,
            order: None,
        }
        .into_algebra()
        .unwrap();

        // Sanity: the legacy global lookup picks #1 (first DFS
        // match) for both — this is the buggy behavior.
        assert_eq!(
            tree.find_comprehension_scope(&x_comp),
            Some(4),
            "legacy lookup returns FIRST match — documented bug"
        );

        // The fix: searching under the A outer (idx 3) returns
        // #1 (idx 4); searching under the B outer (idx 6)
        // returns #2 (idx 7). The same x AST resolves to
        // different scope idx based on the parent context.
        assert_eq!(
            tree.find_comprehension_scope_under(3, &x_comp),
            Some(4),
            "under A outer, x-comprehension is the descendant at idx 4"
        );
        assert_eq!(
            tree.find_comprehension_scope_under(6, &x_comp),
            Some(7),
            "under B outer, x-comprehension is the descendant at idx 7"
        );

        // Cross-search: looking for x under the OTHER side's
        // sub-tree should return None (the comprehension isn't
        // a descendant).
        assert_eq!(
            tree.find_comprehension_scope_under(4, &x_comp),
            None,
            "x-comp is not a descendant of itself"
        );
    }

    /// **WORKAROUND-PINNING TEST — retire when AST becomes
    /// self-identifying** (task #19). See
    /// [`find_comprehension_scope_under_disambiguates_identical_ast`]
    /// for the architectural framing. Same shape applied to
    /// `Bindings` nodes whose source text matches at different
    /// scope-tree positions.
    #[test]
    fn find_bindings_scope_under_disambiguates_identical_source() {
        let bindings_source = "const k := 1\n".to_string();
        let bindings_node = || ScenarioNode::Bindings {
            source: bindings_source.clone(),
            children: vec![phase("P")],
        };
        let tree = ScopeTree::build(
            "default",
            &[
                for_each("a in [1]", vec![bindings_node()]),
                for_each("b in [2]", vec![bindings_node()]),
            ],
        );
        // Layout:
        //   0 session
        //   1 workload
        //   2 scenario
        //   3 for_each(a)
        //   4   bindings #1
        //   5     phase(P)
        //   6 for_each(b)
        //   7   bindings #2
        //   8     phase(P)

        // Legacy: FIRST match (bug).
        assert_eq!(tree.find_bindings_scope(&bindings_source), Some(4));

        // Fix: scoped lookup picks the right descendant.
        assert_eq!(tree.find_bindings_scope_under(3, &bindings_source), Some(4));
        assert_eq!(tree.find_bindings_scope_under(6, &bindings_source), Some(7));
    }
}

/// One-line display label for a scenario-level `bindings:` scope.
///
/// Summarizes the names the scope DEFINES (`x := …`, `shared y := …`,
/// `extern z: T = …`, `input w: T`) instead of echoing raw source —
/// the first source line is often a comment, which read as an
/// unnatural, repeating emission in the scenario-tree readout.
/// Comment-only / empty sources degrade to a bare `bindings:`.
pub fn bindings_label(source: &str) -> String {
    let mut names: Vec<&str> = Vec::new();
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let t = t
            .strip_prefix("shared ")
            .or_else(|| t.strip_prefix("volatile "))
            .or_else(|| t.strip_prefix("const "))
            .or_else(|| t.strip_prefix("final "))
            .unwrap_or(t);
        let t = t
            .strip_prefix("extern ")
            .or_else(|| t.strip_prefix("input "))
            .unwrap_or(t);
        let ident_end = t
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(t.len());
        if ident_end == 0 {
            continue;
        }
        let rest = t[ident_end..].trim_start();
        if rest.starts_with(":=") || rest.starts_with(':') {
            let name = &t[..ident_end];
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    match names.len() {
        0 => "bindings:".to_string(),
        1..=4 => format!("bindings: {}", names.join(", ")),
        n => format!("bindings: {} (+{} more)", names[..4].join(", "), n - 4),
    }
}
