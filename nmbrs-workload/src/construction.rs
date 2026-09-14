// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Enumerable workload-construction nodes.
//!
//! Every node kind of the workload grammar (workload root,
//! phase, op, poll, scenario node, …) implements
//! [`EnumerableNode`]: it names itself with a single
//! discriminant, enumerates its valid sub-elements WITH their
//! valid forms, and VALIDATES a runtime AST (partial or
//! complete) directly. Child node kinds are held as DIRECT
//! `&'static dyn EnumerableNode` references inside [`Form`] — no
//! side mapping from names to nodes exists, so a dangling
//! reference is unrepresentable: the reference graph is checked
//! by the compiler, the same single-discriminant discipline the
//! wrapper registry uses at the execution-context layer.
//!
//! These tables are the single declaration point for each
//! element (name + forms + doc together). The flat name lists in
//! [`crate::vocab`] are PROJECTIONS of these tables (the parser
//! consumes those projections for its rejection checks), and the
//! phase table is pinned against the
//! [`crate::model::WorkloadPhase`] struct itself by a serde
//! field probe in this module's tests.
//!
//! ENFORCEMENT: `parse_workload` runs [`validate_workload`] on
//! every extends-merged document (the construction gate —
//! docs/guide/construction_model.md). COHERENCE AXIOM: a
//! construction that cannot be represented in the synthesizer
//! fuzz is not yet a valid construction; the fuzzer's deferral
//! list is the debt register against this axiom.
//!
//! Context sensitivity: [`EnumerableNode::elements`] receives
//! the partial AST node (when the caller has one) and may narrow
//! — e.g. a phase carrying `poll:` pins `concurrency` to 1, a
//! phase with inline `ops:` loses the `tags:` selector (one
//! source of ops, SRD-108 Part A), and an op with `abstract:`
//! drops the statement payload.

use serde_json::Value;

/// A valid value form for one element. Nested node kinds are
/// direct references — the grammar graph carries itself.
#[derive(Clone, Copy)]
pub enum Form {
    /// Boolean (elements document accepted 0/1/"on"/"off" sugar).
    Bool,
    /// Unsigned integer (numeric strings accepted where the
    /// element documents it).
    U64,
    /// Float.
    F64,
    /// Plain string.
    Str,
    /// List of strings (globs where documented).
    StrList,
    /// Duration: `"2.5h"`, bare seconds, or a `{param}` ref.
    Duration,
    /// A `{param}` / iter-var reference accepted alongside the
    /// literal forms listed beside it.
    ParamRef,
    /// A polydat bindings SOURCE block (multi-line grammar).
    GkSource,
    /// A single polydat expression (predicates, metric values).
    GkExpr,
    /// A capture / result path expression (`/0/field`,
    /// `rows[*].col` — SRD-70).
    PathExpr,
    /// A metric selector (`"family, label=value"`).
    MetricSelector,
    /// One of a closed vocabulary of literal values.
    Vocab(&'static [&'static str]),
    /// A nested node of the referenced kind.
    Node(&'static dyn EnumerableNode),
    /// A map of AUTHOR-CHOSEN names, each value a node of the
    /// referenced kind (e.g. `phases:` → phase nodes).
    NamedMap(&'static dyn EnumerableNode),
    /// A list whose entries are nodes of the referenced kind.
    ListOf(&'static dyn EnumerableNode),
    /// A map of author-chosen names whose VALUES all take the
    /// given form (e.g. `capture:` name → PathExpr).
    MapOf(&'static Form),
    /// A list whose entries all take the given form.
    ListOfForm(&'static Form),
    /// An open map (author-chosen keys, unmodeled values).
    FreeMap,
    /// An open scalar (adapter-defined or free text).
    FreeScalar,
}

impl std::fmt::Debug for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Form::Bool => write!(f, "Bool"),
            Form::U64 => write!(f, "U64"),
            Form::F64 => write!(f, "F64"),
            Form::Str => write!(f, "Str"),
            Form::StrList => write!(f, "StrList"),
            Form::Duration => write!(f, "Duration"),
            Form::ParamRef => write!(f, "ParamRef"),
            Form::GkSource => write!(f, "GkSource"),
            Form::GkExpr => write!(f, "GkExpr"),
            Form::PathExpr => write!(f, "PathExpr"),
            Form::MetricSelector => write!(f, "MetricSelector"),
            Form::Vocab(v) => write!(f, "Vocab({v:?})"),
            Form::Node(n) => write!(f, "Node({})", n.kind()),
            Form::NamedMap(n) => write!(f, "NamedMap({})", n.kind()),
            Form::ListOf(n) => write!(f, "ListOf({})", n.kind()),
            Form::MapOf(inner) => write!(f, "MapOf({inner:?})"),
            Form::ListOfForm(inner) => write!(f, "ListOfForm({inner:?})"),
            Form::FreeMap => write!(f, "FreeMap"),
            Form::FreeScalar => write!(f, "FreeScalar"),
        }
    }
}

impl PartialEq for Form {
    fn eq(&self, other: &Self) -> bool {
        use Form::*;
        match (self, other) {
            (Vocab(a), Vocab(b)) => a == b,
            (Node(a), Node(b)) | (NamedMap(a), NamedMap(b)) | (ListOf(a), ListOf(b)) => {
                a.kind() == b.kind()
            }
            (MapOf(a), MapOf(b)) | (ListOfForm(a), ListOfForm(b)) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

/// One valid sub-element of a node: the single declaration point
/// for its name, accepted forms, and documentation.
#[derive(Debug, Clone, Copy)]
pub struct ElementSpec {
    pub name: &'static str,
    pub forms: &'static [Form],
    pub required: bool,
    /// Serde-alias spellings that satisfy this element (a
    /// REQUIRED element is present when its name OR any alias
    /// key is).
    pub aliases: &'static [&'static str],
    pub doc: &'static str,
}

const fn el(name: &'static str, forms: &'static [Form], doc: &'static str) -> ElementSpec {
    ElementSpec {
        name,
        forms,
        required: false,
        aliases: &[],
        doc,
    }
}

/// Validation strictness: a PARTIAL model skips required-element
/// checks (it is still being authored); a COMPLETE model does
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Partial,
    Complete,
}

/// One validation finding, with the AST path it anchors to.
#[derive(Debug, Clone)]
pub struct Violation {
    pub path: String,
    pub message: String,
}

/// An enumerable node kind of the workload construction grammar.
pub trait EnumerableNode: Sync {
    /// Canonical kind discriminant (`"workload"`, `"phase"`, …).
    fn kind(&self) -> &'static str;
    /// One-line description of the node kind.
    fn doc(&self) -> &'static str;
    /// Valid sub-elements. `partial` is the (possibly incomplete)
    /// AST value AT this node when the caller has one; nodes may
    /// narrow their element set from it. `None` yields the
    /// unconditioned surface.
    fn elements(&self, partial: Option<&Value>) -> Vec<ElementSpec>;
    /// When this node also accepts keys OUTSIDE the enumerated
    /// set, the reason (open vocabularies are declared, never
    /// implied). `None` = closed surface.
    fn open_surface(&self) -> Option<&'static str> {
        None
    }

    /// Validate `value` as an instance of this node kind:
    /// unknown keys on closed surfaces, form conformance per
    /// element, and (in [`Mode::Complete`]) required elements —
    /// recursing through nested node references. This is the
    /// runtime type check of the construction grammar.
    fn validate(&self, value: &Value, mode: Mode) -> Vec<Violation>
    where
        Self: Sized,
    {
        let mut out = Vec::new();
        validate_node(self, value, mode, self.kind(), &mut out);
        out
    }
}

// ─── Node statics (single discriminants, referenced directly) ────

pub static WORKLOAD: WorkloadNode = WorkloadNode;
pub static PHASE: PhaseNode = PhaseNode;
pub static OP: OpNode = OpNode;
pub static POLL: PollNode = PollNode;
pub static SCENARIO: ScenarioNode = ScenarioNode;
pub static STOP_WHEN: StopWhenNode = StopWhenNode;
pub static METRIC: MetricNode = MetricNode;
pub static ABSTRACT: AbstractNode = AbstractNode;
pub static EVALUATIONS: EvaluationsNode = EvaluationsNode;
pub static RELEVANCY: RelevancyNode = RelevancyNode;
pub static BINDING_CLASSES: BindingClassesNode = BindingClassesNode;
pub static MEMO: MemoNode = MemoNode;
pub static OP_POLL: OpPollNode = OpPollNode;
pub static TRIES: TriesNode = TriesNode;
pub static BACKOFF: BackoffNode = BackoffNode;
pub static THROTTLE: ThrottleNode = ThrottleNode;
pub static DELAY: DelayNode = DelayNode;
pub static CONTINUE_IF: ContinueIfNode = ContinueIfNode;
pub static CHECKPOINT: CheckpointNode = CheckpointNode;
pub static OPTIMIZE: OptimizeNode = OptimizeNode;

/// Every registered node kind — an enumeration surface for
/// tooling (`describe`, docs). Resolution never goes through
/// this list: forms hold their child nodes directly.
pub static ALL_NODES: &[&dyn EnumerableNode] = &[
    &WORKLOAD,
    &PHASE,
    &OP,
    &POLL,
    &SCENARIO,
    &STOP_WHEN,
    &METRIC,
    &ABSTRACT,
    &EVALUATIONS,
    &RELEVANCY,
    &BINDING_CLASSES,
    &MEMO,
    &OP_POLL,
    &TRIES,
    &BACKOFF,
    &THROTTLE,
    &DELAY,
    &CONTINUE_IF,
    &CHECKPOINT,
    &OPTIMIZE,
];

/// Kind lookup over [`ALL_NODES`] — a convenience for external
/// tooling only.
pub fn node_by_kind(kind: &str) -> Option<&'static dyn EnumerableNode> {
    ALL_NODES.iter().copied().find(|n| n.kind() == kind)
}

/// The workload root node.
pub fn root() -> &'static dyn EnumerableNode {
    &WORKLOAD
}

// ─── Spec tables (single declaration points) ─────────────────────

const STOP_EFFECTS: &[&str] = &["stop", "fail", "abort"];

pub static PHASE_ELEMENTS: &[ElementSpec] = &[
    el(
        "cycles",
        &[Form::U64, Form::ParamRef, Form::Str],
        "stanza count; string forms: {param}, extent sigils (==ops:N, ===auto) — runtime-parsed",
    ),
    el(
        "concurrency",
        &[Form::U64, Form::ParamRef, Form::Str],
        "async fibers ({param} or bare wire name resolves in scope)",
    ),
    el(
        "rate",
        &[Form::F64, Form::ParamRef],
        "ops/sec; {param}/iter-var resolves at the phase gather",
    ),
    el(
        "daemon",
        &[Form::Bool],
        "runs concurrently with foreground siblings; stopped when they complete",
    ),
    el("adapter", &[Form::Str], "adapter override for this phase"),
    el(
        "errors",
        &[Form::Str],
        "error-routing spec (e.g. \"count,retry\", \".*:counter\")",
    ),
    el(
        "tries",
        &[Form::U64, Form::Node(&TRIES)],
        "total attempts; map form adds backoff (carries tries_backoff)",
    ),
    el(
        "interval",
        &[Form::Duration],
        "declarative-only today: phase re-run pacing",
    ),
    el("repeat", &[Form::U64], "bound for interval"),
    el(
        "error_rate_max",
        &[Form::F64],
        "opt-in error-rate circuit breaker",
    ),
    el(
        "timeout",
        &[Form::Duration, Form::ParamRef],
        "governance bound; expiry = Interrupted+Failed (timeout)",
    ),
    el(
        "stop_when",
        &[Form::ListOf(&STOP_WHEN)],
        "SRD-83 stop conditions",
    ),
    el(
        "throttle",
        &[Form::Bool, Form::Node(&THROTTLE)],
        "adaptive backpressure governor: windowed attempt-failure fraction walks a dynamic control (true = defaults)",
    ),
    el(
        "tags",
        &[Form::Str],
        "tag FILTER selecting ops from blocks (exclusive with inline ops)",
    ),
    el(
        "ops",
        &[Form::NamedMap(&OP), Form::ListOf(&OP)],
        "inline op templates (exclusive with tags selector)",
    ),
    el("for_each", &[Form::Str], "phase sweep: \"var in expr\""),
    el(
        "continue_if",
        &[Form::GkExpr, Form::Node(&CONTINUE_IF)],
        "pre-entry gate bounding a for_each sweep",
    ),
    el(
        "loop_scope",
        &[Form::Vocab(&["clean", "inherit"])],
        "loop-context seeding for for_each",
    ),
    el(
        "iter_scope",
        &[Form::Vocab(&["clean", "inherit"])],
        "iteration seeding for for_each",
    ),
    el(
        "checkpoint",
        &[
            Form::Vocab(&["idempotent", "disabled"]),
            Form::Node(&CHECKPOINT),
        ],
        "skip-on-resume eligibility (SRD-44/106)",
    ),
    el(
        "status_metrics",
        &[Form::StrList],
        "metric-name globs surfaced on the status line",
    ),
    el(
        "bindings",
        &[Form::GkSource, Form::FreeMap],
        "phase-scope polydat bindings (map = legacy form)",
    ),
    el(
        "metrics",
        &[
            Form::NamedMap(&METRIC),
            Form::GkExpr,
            Form::ListOfForm(&Form::GkExpr),
        ],
        "completion-time phase metrics (SRD-40b sugars: bare expr / list)",
    ),
    el(
        "dimensions",
        &[Form::FreeMap],
        "label declarations owned by this tier",
    ),
    el(
        "poll",
        &[Form::Node(&POLL)],
        "SRD-75 phase-poll loop (forbids concurrency > 1)",
    ),
    el(
        "optimize",
        &[Form::GkExpr, Form::Node(&OPTIMIZE)],
        "SRD-86 optimizer dispatch (string = objective sugar)",
    ),
    el(
        "key_metrics",
        &[Form::FreeMap],
        "SRD-113 key-metric designations: column -> agg(family), aggregate mandatory",
    ),
];

pub static OP_MODEL_ELEMENTS: &[ElementSpec] = &[
    el("name", &[Form::Str], "implicit — set by the op's map key"),
    el("description", &[Form::Str], "op description"),
    el("desc", &[Form::Str], "legacy alias of description"),
    el(
        "bindings",
        &[Form::GkSource, Form::FreeMap],
        "op-scope polydat bindings (map = legacy form)",
    ),
    el(
        "params",
        &[Form::FreeMap],
        "activity-level params excised from adapter fields",
    ),
    el("tags", &[Form::FreeMap], "tags for filtering and metadata"),
    el(
        "if",
        &[Form::GkExpr],
        "per-cycle condition; falsy skips the op",
    ),
    el(
        "delay",
        &[Form::Str, Form::Node(&DELAY)],
        "pre/post-op delay wire name(s)",
    ),
    el(
        "evaluations",
        &[Form::Node(&EVALUATIONS)],
        "closed-vocab validation/scoring wrapper",
    ),
    el(
        "capture",
        &[Form::MapOf(&Form::PathExpr)],
        "wire ← path-expression extraction map",
    ),
    el(
        "metrics",
        &[
            Form::NamedMap(&METRIC),
            Form::GkExpr,
            Form::ListOfForm(&Form::GkExpr),
        ],
        "per-cycle synthetic metrics (SRD-40b sugars: bare expr / list)",
    ),
    el(
        "result",
        &[Form::FreeMap, Form::GkSource],
        "SRD-66 result bindings (named paths / source)",
    ),
    el(
        "traverse",
        &[Form::FreeMap],
        "customizes the result traversal layer",
    ),
    el(
        "measure",
        &[Form::FreeMap, Form::Str, Form::ParamRef],
        "measurement config (string = template form)",
    ),
    el(
        "abstract",
        &[Form::Node(&ABSTRACT)],
        "SRD-108 typed interface slot (blueprint side)",
    ),
    el(
        "daemon",
        &[Form::Bool, Form::U64],
        "op-level daemon fiber (N = max concurrent)",
    ),
    el(
        "daemon_cancel_grace_ms",
        &[Form::U64],
        "daemon cancel grace",
    ),
    el("while", &[Form::GkExpr], "op-level loop guard"),
    el(
        "rate",
        &[Form::F64, Form::Str],
        "op-level pacing (number or rate string like \"5/s\")",
    ),
];

/// Activity-level op keys excised from adapter fields by the
/// model layer (the parser's excision list is this table's name
/// projection — single source).
pub static OP_ACTIVITY_ELEMENTS: &[ElementSpec] = &[
    el("ratio", &[Form::U64], "op dispatch ratio within the stanza"),
    el("adapter", &[Form::Str], "adapter override for this op"),
    el("driver", &[Form::Str], "driver alias override"),
    el(
        "space",
        &[Form::Str, Form::ParamRef],
        "adapter space selector",
    ),
    el("instrument", &[Form::Bool], "per-op instrumentation toggle"),
    el("start-timers", &[Form::StrList], "timer starts"),
    el("stop-timers", &[Form::StrList], "timer stops"),
    el(
        "verify",
        &[
            Form::ListOfForm(&Form::FreeMap),
            Form::FreeMap,
            Form::GkExpr,
        ],
        "response verification clauses (string = predicate expression)",
    ),
    el(
        "relevancy",
        &[Form::Node(&RELEVANCY)],
        "legacy top-level shorthand for evaluations.relevancy",
    ),
    el("strict", &[Form::Bool], "strict verification"),
    el(
        "poll",
        &[Form::Node(&OP_POLL)],
        "op-level poll loop (loops ONLY this op)",
    ),
    el("poll_interval_ms", &[Form::U64], "legacy op-poll interval"),
    el("timeout_ms", &[Form::U64], "per-op timeout"),
    el(
        "poll_metric_name",
        &[Form::Str],
        "legacy op-poll metric name",
    ),
    el("emit", &[Form::Str], "emission override"),
    el(
        "batch",
        &[Form::U64, Form::ParamRef, Form::Str],
        "batch row cap: literal, {param}, or bare wire name",
    ),
    el(
        "max_batch_size",
        &[Form::U64, Form::GkExpr],
        "batch byte budget (literal or GK call)",
    ),
    el(
        "batchtype",
        &[Form::Vocab(&["logged", "unlogged", "counter"])],
        "CQL batch type",
    ),
    el(
        "memo",
        &[Form::Node(&MEMO), Form::Str],
        "operator-facing before/after notes",
    ),
    el(
        "gutter",
        &[Form::FreeMap, Form::Str],
        "status-gutter template (runtime-resolved)",
    ),
    el(
        "readout",
        &[Form::Vocab(&["visible", "hidden"])],
        "SRD-63 op-level status visibility",
    ),
    el("errors", &[Form::Str], "per-op error-routing override"),
    el(
        "tries",
        &[Form::U64, Form::Node(&TRIES)],
        "per-op total-attempts sigil",
    ),
];

pub static OP_STMT_ELEMENTS: &[ElementSpec] = &[
    el("stmt", &[Form::FreeScalar], "canonical statement payload"),
    el("op", &[Form::FreeScalar], "statement payload alias"),
    el("ops", &[Form::FreeScalar], "statement payload alias"),
    el("operations", &[Form::FreeScalar], "statement payload alias"),
    el("statement", &[Form::FreeScalar], "statement payload alias"),
    el("statements", &[Form::FreeScalar], "statement payload alias"),
];

pub static POLL_ELEMENTS: &[ElementSpec] = &[
    ElementSpec {
        name: "until",
        forms: &[Form::GkExpr],
        required: true,
        aliases: &[],
        doc: "predicate over captures; re-evaluated per iteration",
    },
    el(
        "interval_ms",
        &[Form::U64, Form::ParamRef],
        "sleep between iterations (default 1000)",
    ),
    el(
        "timeout_ms",
        &[Form::U64, Form::ParamRef],
        "wall-clock cap (default 300000)",
    ),
    el(
        "max_error_retries",
        &[Form::U64, Form::ParamRef],
        "tolerated consecutive retryable errors",
    ),
    el(
        "metric_name",
        &[Form::Str],
        "gauge written on successful loop exit (unit from suffix)",
    ),
    el(
        "on_timeout",
        &[Form::Vocab(&["error", "abort"])],
        "deadline disposition",
    ),
    el(
        "require",
        &[Form::MetricSelector, Form::StrList],
        "strict-gate selectors that must resolve",
    ),
];

pub static SCENARIO_NODE_ELEMENTS: &[ElementSpec] = &[
    el(
        "for_each",
        &[Form::Str, Form::StrList],
        "iteration: \"var in expr\" (list = union of sub-spaces)",
    ),
    el(
        "for",
        &[Form::Str, Form::StrList],
        "alias of for_each (list = union of sub-spaces)",
    ),
    el(
        "anchor",
        &[Form::Str],
        "SRD-113 view anchor: one synthesized table row per iteration",
    ),
    el("scenario", &[Form::Str], "include another scenario by name"),
    el(
        "scenarios",
        &[Form::Str, Form::ListOf(&SCENARIO)],
        "scenario include(s) — plural takes a heterogeneous list",
    ),
    el(
        "for_combinations",
        &[Form::FreeMap],
        "cross-product map form: var → expr",
    ),
    el(
        "do_while",
        &[Form::GkExpr],
        "run children while true (test after)",
    ),
    el(
        "do_until",
        &[Form::GkExpr],
        "run children until true (test after)",
    ),
    el(
        "bindings",
        &[Form::GkSource, Form::FreeMap],
        "scenario-node bindings",
    ),
    el("set", &[Form::FreeMap], "param shadows over the child tree"),
    el(
        "phases",
        &[Form::ListOf(&SCENARIO)],
        "child nodes under a structural key",
    ),
    el(
        "counter",
        &[Form::Str],
        "loop counter wire for do_while / do_until",
    ),
];

pub static STOP_WHEN_ELEMENTS: &[ElementSpec] = &[
    ElementSpec {
        name: "when",
        forms: &[Form::GkExpr, Form::ParamRef],
        required: true,
        aliases: &["condition"],
        doc: "predicate over runtime-state wires; {param} interpolates",
    },
    el(
        "condition",
        &[Form::GkExpr, Form::ParamRef],
        "serde alias of when",
    ),
    el(
        "effect",
        &[Form::Vocab(STOP_EFFECTS)],
        "disposition on fire (alias: action; default fail)",
    ),
    el(
        "action",
        &[Form::Vocab(STOP_EFFECTS)],
        "canonical key for effect",
    ),
    el(
        "each",
        &[Form::StrList, Form::Str],
        "detection scope selector(s)",
    ),
    el("per", &[Form::StrList, Form::Str], "serde alias of each"),
    el("trigger", &[Form::Str], "firing trigger"),
    el("pulse", &[Form::FreeMap, Form::Str], "firing axis"),
    el(
        "at",
        &[Form::Str],
        "ACTION target scope — where the effect lands",
    ),
];

pub static METRIC_ELEMENTS: &[ElementSpec] = &[
    ElementSpec {
        name: "value",
        forms: &[Form::GkExpr],
        required: true,
        aliases: &[],
        doc: "polydat expression over in-scope wires",
    },
    el(
        "family",
        &[Form::Str],
        "family override (default: the metric's map key)",
    ),
    el(
        "kind",
        &[Form::Vocab(&["gauge", "counter", "histogram"])],
        "instrument kind",
    ),
    el("unit", &[Form::Str], "unit hint"),
    el("format", &[Form::Str], "render format hint"),
    el(
        "cell",
        &[Form::FreeMap],
        "dimension placement: label → wire (SRD coordinate cells)",
    ),
];

pub static ABSTRACT_ELEMENTS: &[ElementSpec] = &[
    el(
        "needs",
        &[Form::MapOf(&Form::FreeScalar)],
        "wires the blueprint GUARANTEES: name → port-type keyword",
    ),
    el(
        "yields",
        &[Form::MapOf(&Form::FreeScalar)],
        "wires the implementation must CAPTURE: name → port-type keyword",
    ),
    el(
        "results",
        &[Form::MapOf(&Form::FreeScalar)],
        "wires delivered via result: projections: name → port-type keyword",
    ),
];

pub static EVALUATIONS_ELEMENTS: &[ElementSpec] = &[
    el(
        "relevancy",
        &[Form::Node(&RELEVANCY)],
        "recall/ndcg scoring against ground truth",
    ),
    el("verify", &[Form::FreeMap], "field-equality verification"),
];

pub static RELEVANCY_ELEMENTS: &[ElementSpec] = &[
    ElementSpec {
        name: "actual",
        forms: &[Form::Str],
        required: true,
        aliases: &[],
        doc: "wire carrying retrieved values (bare wire name)",
    },
    ElementSpec {
        name: "expected",
        forms: &[Form::Str],
        required: true,
        aliases: &[],
        doc: "wire carrying ground truth (bare wire name)",
    },
    el(
        "k",
        &[Form::U64, Form::Str],
        "evaluation depth (literal or bare wire name)",
    ),
    el(
        "r",
        &[Form::U64, Form::Str],
        "retrieved window (literal or bare wire name)",
    ),
    el(
        "functions",
        &[Form::ListOfForm(&Form::Vocab(&["recall", "ndcg"]))],
        "scoring functions",
    ),
];

pub static MEMO_ELEMENTS: &[ElementSpec] = &[
    el(
        "before",
        &[Form::Str, Form::ParamRef],
        "note before dispatch ({wire} interpolation)",
    ),
    el(
        "after",
        &[Form::Str, Form::ParamRef],
        "note after completion ({wire} interpolation)",
    ),
];

pub static OP_POLL_ELEMENTS: &[ElementSpec] = &[
    el(
        "mode",
        &[Form::Vocab(&["await_empty"])],
        "loop-until mode over the result body",
    ),
    el("until", &[Form::GkExpr], "predicate alternative to mode"),
    el("interval_ms", &[Form::U64], "sleep between iterations"),
    el("timeout_ms", &[Form::U64], "wall-clock cap"),
    el(
        "max_error_retries",
        &[Form::U64],
        "tolerated consecutive retryable errors",
    ),
    el("metric_name", &[Form::Str], "gauge on successful exit"),
    el(
        "memo",
        &[Form::Str, Form::ParamRef, Form::Node(&MEMO)],
        "per-iteration operator note ({wire} interpolation)",
    ),
    el(
        "progress",
        &[Form::Str, Form::ParamRef],
        "derived-progress template overriding the cycle bar",
    ),
];

pub static TRIES_ELEMENTS: &[ElementSpec] = &[
    el("count", &[Form::U64], "total attempts"),
    el(
        "backoff",
        &[Form::Node(&BACKOFF)],
        "retry backoff overrides",
    ),
];

pub static THROTTLE_ELEMENTS: &[ElementSpec] = &[
    el(
        "high",
        &[Form::F64],
        "windowed attempt-failure fraction that triggers back-off (default 0.05)",
    ),
    el("low", &[Form::F64], "recovery threshold (default high/5)"),
    el(
        "control",
        &[Form::Vocab(&["concurrency", "rate"])],
        "the dynamic control the governor walks",
    ),
    el(
        "start",
        &[Form::F64],
        "initial offered value (slow-start seed; default = floor — declare higher only for known-robust targets)",
    ),
    el("floor", &[Form::F64], "never throttle below this value"),
    el(
        "window",
        &[Form::Duration],
        "evaluation window (default 2s)",
    ),
];

pub static BACKOFF_ELEMENTS: &[ElementSpec] = &[
    el("ratio", &[Form::F64], "growth ratio"),
    el("min", &[Form::Duration], "floor"),
    el("max", &[Form::Duration], "ceiling"),
];

pub static DELAY_ELEMENTS: &[ElementSpec] = &[
    el("before", &[Form::Str], "pre-op delay wire name"),
    el("after", &[Form::Str], "post-op delay wire name"),
];

pub static CONTINUE_IF_ELEMENTS: &[ElementSpec] = &[
    ElementSpec {
        name: "when",
        forms: &[Form::GkExpr],
        required: true,
        aliases: &["condition"],
        doc: "pre-entry predicate (bare wires via for_iteration scope)",
    },
    el("condition", &[Form::GkExpr], "serde alias of when"),
    el(
        "each",
        &[Form::StrList, Form::Str],
        "evaluation scope selector(s)",
    ),
    el("per", &[Form::StrList, Form::Str], "serde alias of each"),
];

pub static CHECKPOINT_ELEMENTS: &[ElementSpec] = &[
    el("idempotent", &[Form::Bool], "prereq-class skip eligibility"),
    el("hashed", &[Form::Bool], "provenance hashing toggle"),
    el(
        "verify",
        &[Form::FreeMap],
        "verify op-template body run before Skip",
    ),
];

pub static OPTIMIZE_ELEMENTS: &[ElementSpec] = &[
    el("method", &[Form::Str], "optimizer method (default sweep)"),
    ElementSpec {
        name: "objective",
        forms: &[Form::GkExpr],
        required: true,
        aliases: &[],
        doc: "objective wire expression (string sugar = whole block)",
    },
    el("servo", &[Form::StrList], "servo axis wires"),
    el("max_evals", &[Form::U64], "evaluation budget"),
    el("seed", &[Form::U64], "search seed"),
];

pub static WORKLOAD_ELEMENTS: &[ElementSpec] = &[
    el(
        "description",
        &[Form::Str],
        "workload description (required for curated catalog entries)",
    ),
    el(
        "extends",
        &[Form::Str],
        "parent document (path or catalog name)",
    ),
    el(
        "implements",
        &[Form::Str],
        "blueprint this implementation binds (SRD-108)",
    ),
    el(
        "stick_session",
        &[Form::Bool],
        "SRD-106 re-attach-by-default",
    ),
    el("params", &[Form::FreeMap], "declared params with defaults"),
    el(
        "bindings",
        &[Form::GkSource, Form::FreeMap],
        "workload-root polydat bindings (map = legacy name→expr form)",
    ),
    el("phases", &[Form::NamedMap(&PHASE)], "phase definitions"),
    el("scenarios", &[Form::NamedMap(&SCENARIO)], "scenario trees"),
    el(
        "ops",
        &[Form::NamedMap(&OP), Form::ListOf(&OP)],
        "top-level ops (legacy inline shape)",
    ),
    el(
        "op",
        &[Form::FreeScalar, Form::FreeMap],
        "inline single-op shorthand",
    ),
    el(
        "blocks",
        &[Form::FreeMap],
        "named op blocks for tag selection",
    ),
    el("tags", &[Form::FreeMap], "document tags"),
    el(
        "stop_when",
        &[Form::ListOf(&STOP_WHEN)],
        "workload-shell stop conditions",
    ),
    el(
        "status_metrics",
        &[Form::StrList],
        "doc-root status-line default",
    ),
    el(
        "report",
        &[Form::FreeMap],
        "SRD-46 report block (directive grammar: crate::report::vocab)",
    ),
    el("readouts", &[Form::FreeMap], "SRD-63 readout slot bindings"),
    el(
        "wrappers",
        &[Form::FreeMap],
        "SRD-32a wrapper-order override",
    ),
];

/// GK binding declaration classes usable from workload
/// `bindings:` blocks (the grammar itself is polydat's; the bare
/// `name := expr` derived form has no keyword).
pub static BINDING_CLASS_ELEMENTS: &[ElementSpec] = &[
    el("const", &[Form::GkSource], "construction-time constant"),
    el(
        "cursor",
        &[Form::GkSource],
        "extent-driving cursor declaration",
    ),
    el(
        "volatile",
        &[Form::GkSource],
        "re-evaluated per pull; acknowledges non-determinism",
    ),
    el(
        "shared",
        &[Form::GkSource],
        "phase-scope shared cell (write-through across ops)",
    ),
    el(
        "extern",
        &[Form::GkSource],
        "externally-written slot (captures, injected wires)",
    ),
    el(
        "input",
        &[Form::GkSource],
        "kernel coordinate input (kernel-plane; not workload-authored)",
    ),
];

// ─── Node implementations ─────────────────────────────────────────

macro_rules! simple_node {
    ($ty:ident, $kind:literal, $doc:literal, $table:ident) => {
        pub struct $ty;
        impl EnumerableNode for $ty {
            fn kind(&self) -> &'static str {
                $kind
            }
            fn doc(&self) -> &'static str {
                $doc
            }
            fn elements(&self, _partial: Option<&Value>) -> Vec<ElementSpec> {
                $table.to_vec()
            }
        }
    };
}

simple_node!(
    WorkloadNode,
    "workload",
    "workload document root",
    WORKLOAD_ELEMENTS
);
simple_node!(PollNode, "poll", "SRD-75 phase-poll loop", POLL_ELEMENTS);
pub struct ScenarioNode;
impl EnumerableNode for ScenarioNode {
    fn kind(&self) -> &'static str {
        "scenario"
    }
    fn doc(&self) -> &'static str {
        "scenario-tree node (bare string = phase name; object = structural node)"
    }
    fn elements(&self, _partial: Option<&Value>) -> Vec<ElementSpec> {
        SCENARIO_NODE_ELEMENTS.to_vec()
    }
    fn open_surface(&self) -> Option<&'static str> {
        Some(
            "legacy command-string entries (`name: \"run …\"`) — string \
              values pass through; structural keys are closed and the \
              parser rejects unknown ones with non-string values",
        )
    }
}
simple_node!(
    StopWhenNode,
    "stop_when",
    "SRD-83 stop condition",
    STOP_WHEN_ELEMENTS
);
simple_node!(
    MetricNode,
    "metric",
    "synthetic metric declaration (SRD-40b)",
    METRIC_ELEMENTS
);
simple_node!(
    AbstractNode,
    "abstract",
    "SRD-108 typed interface slot",
    ABSTRACT_ELEMENTS
);
simple_node!(
    EvaluationsNode,
    "evaluations",
    "post-execution scoring wrapper",
    EVALUATIONS_ELEMENTS
);
simple_node!(
    RelevancyNode,
    "relevancy",
    "recall/ndcg scoring config",
    RELEVANCY_ELEMENTS
);
simple_node!(
    BindingClassesNode,
    "binding_classes",
    "GK binding declaration classes (grammar lives in polydat)",
    BINDING_CLASS_ELEMENTS
);
simple_node!(
    MemoNode,
    "memo",
    "operator-facing memo notes",
    MEMO_ELEMENTS
);
simple_node!(
    OpPollNode,
    "op_poll",
    "op-level poll loop",
    OP_POLL_ELEMENTS
);
simple_node!(TriesNode, "tries", "tries map form", TRIES_ELEMENTS);
simple_node!(
    BackoffNode,
    "backoff",
    "retry backoff overrides",
    BACKOFF_ELEMENTS
);
simple_node!(
    ThrottleNode,
    "throttle",
    "adaptive backpressure governor",
    THROTTLE_ELEMENTS
);
simple_node!(DelayNode, "delay", "pre/post-op delay", DELAY_ELEMENTS);
simple_node!(
    ContinueIfNode,
    "continue_if",
    "SRD-101 pre-entry gate",
    CONTINUE_IF_ELEMENTS
);
simple_node!(
    CheckpointNode,
    "checkpoint",
    "checkpoint declaration map form",
    CHECKPOINT_ELEMENTS
);
simple_node!(
    OptimizeNode,
    "optimize",
    "SRD-86 optimize block",
    OPTIMIZE_ELEMENTS
);

pub struct PhaseNode;
impl EnumerableNode for PhaseNode {
    fn kind(&self) -> &'static str {
        "phase"
    }
    fn doc(&self) -> &'static str {
        "one measured phase"
    }
    fn elements(&self, partial: Option<&Value>) -> Vec<ElementSpec> {
        let mut out: Vec<ElementSpec> = PHASE_ELEMENTS.to_vec();
        if let Some(Value::Object(map)) = partial {
            // SRD-75: a poll phase is a serial-cycle loop —
            // concurrency is pinned to 1.
            if map.contains_key("poll") {
                for e in &mut out {
                    if e.name == "concurrency" {
                        e.forms = &[Form::Vocab(&["1"])];
                        e.doc = "pinned to 1 under poll: (SRD-75 serial-cycle loop)";
                    }
                }
            }
            // SRD-108 Part A: exactly one source of ops — inline
            // `ops:` and the `tags:` selector are exclusive.
            if map.contains_key("ops") {
                out.retain(|e| e.name != "tags");
            } else if map.contains_key("tags") {
                out.retain(|e| e.name != "ops");
            }
        }
        out
    }
}

pub struct OpNode;
impl EnumerableNode for OpNode {
    fn kind(&self) -> &'static str {
        "op"
    }
    fn doc(&self) -> &'static str {
        "one op template"
    }
    fn elements(&self, partial: Option<&Value>) -> Vec<ElementSpec> {
        let mut out: Vec<ElementSpec> = OP_MODEL_ELEMENTS.to_vec();
        // Activity-level keys share the surface; the `tags` /
        // `errors` / `tries` spellings already covered by the
        // model table keep the model semantics.
        for e in OP_ACTIVITY_ELEMENTS {
            if !out.iter().any(|m| m.name == e.name) {
                out.push(*e);
            }
        }
        let is_abstract = matches!(partial, Some(Value::Object(m)) if m.contains_key("abstract"));
        if !is_abstract {
            // Concrete ops carry a statement payload (one of the
            // alias spellings); abstract slots carry none — the
            // bound implementation contributes it (SRD-108).
            out.extend_from_slice(OP_STMT_ELEMENTS);
        }
        out
    }
    fn open_surface(&self) -> Option<&'static str> {
        Some(
            "adapter op-payload fields — per-adapter surface \
              (closed for http/testkit via known_op_fields, open for cql)",
        )
    }
}

// ─── Runtime type check (validation over any AST) ────────────────

fn form_accepts_scalar(form: &Form, v: &Value) -> bool {
    match form {
        Form::Bool => {
            matches!(v, Value::Bool(_))
                || matches!(v, Value::Number(n) if n.as_u64().is_some_and(|u| u <= 1))
                || matches!(v, Value::String(s)
                if matches!(s.as_str(), "true" | "false" | "on" | "off" | "yes" | "no"))
        }
        Form::U64 => {
            matches!(v, Value::Number(n) if n.as_u64().is_some())
                || matches!(v, Value::String(s) if s.trim().parse::<u64>().is_ok())
        }
        Form::F64 => {
            v.is_number() || matches!(v, Value::String(s) if s.trim().parse::<f64>().is_ok())
        }
        Form::Str
        | Form::GkSource
        | Form::GkExpr
        | Form::PathExpr
        | Form::MetricSelector
        | Form::Duration => v.is_string() || v.is_number(),
        Form::ParamRef => matches!(v, Value::String(s) if s.contains('{') && s.contains('}')),
        Form::StrList => match v {
            Value::Array(items) => items.iter().all(Value::is_string),
            Value::String(_) => true,
            _ => false,
        },
        Form::Vocab(words) => {
            // Match by canonical rendering: strings (trimmed,
            // case-insensitive), numbers, and bools all compare
            // against the vocabulary words.
            let rendered = match v {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return false,
            };
            words.iter().any(|w| w.eq_ignore_ascii_case(&rendered))
        }
        Form::FreeMap => v.is_object(),
        Form::FreeScalar => !v.is_object() || v.is_object(), // anything
        // Structural forms handled by the recursive walk.
        Form::Node(_)
        | Form::NamedMap(_)
        | Form::ListOf(_)
        | Form::MapOf(_)
        | Form::ListOfForm(_) => false,
    }
}

fn validate_against_forms(
    forms: &[Form],
    value: &Value,
    mode: Mode,
    path: &str,
    out: &mut Vec<Violation>,
) {
    // Accept when ANY declared form matches; structural forms
    // recurse and report their own findings.
    for form in forms {
        match form {
            Form::Node(node) => {
                if value.is_object() {
                    validate_node(*node, value, mode, path, out);
                    return;
                }
            }
            Form::NamedMap(node) => {
                if let Value::Object(map) = value {
                    for (name, child) in map {
                        validate_node(*node, child, mode, &format!("{path}.{name}"), out);
                    }
                    return;
                }
            }
            Form::ListOf(node) => {
                if let Value::Array(items) = value {
                    for (i, child) in items.iter().enumerate() {
                        // Scenario lists accept bare-string
                        // shorthands (phase names); only objects
                        // are structural nodes.
                        if child.is_object() {
                            validate_node(*node, child, mode, &format!("{path}[{i}]"), out);
                        }
                    }
                    return;
                }
            }
            Form::MapOf(inner) => {
                if let Value::Object(map) = value {
                    for (name, child) in map {
                        if !form_accepts_scalar(inner, child) {
                            out.push(Violation {
                                path: format!("{path}.{name}"),
                                message: format!("value does not conform to {inner:?}"),
                            });
                        }
                    }
                    return;
                }
            }
            Form::ListOfForm(inner) => {
                if let Value::Array(items) = value {
                    for (i, child) in items.iter().enumerate() {
                        if !form_accepts_scalar(inner, child) && !matches!(**inner, Form::FreeMap) {
                            out.push(Violation {
                                path: format!("{path}[{i}]"),
                                message: format!("entry does not conform to {inner:?}"),
                            });
                        }
                    }
                    return;
                }
            }
            scalar => {
                if form_accepts_scalar(scalar, value) {
                    return;
                }
            }
        }
    }
    out.push(Violation {
        path: path.to_string(),
        message: format!("value conforms to none of the declared forms {forms:?}"),
    });
}

fn validate_node(
    node: &dyn EnumerableNode,
    value: &Value,
    mode: Mode,
    path: &str,
    out: &mut Vec<Violation>,
) {
    if let Value::Array(items) = value {
        // A list instance of a structural kind (scenario trees,
        // list-form op declarations): each object entry is one
        // node; scalar entries are documented shorthands.
        for (i, item) in items.iter().enumerate() {
            if item.is_object() {
                validate_node(node, item, mode, &format!("{path}[{i}]"), out);
            }
        }
        return;
    }
    let Value::Object(map) = value else {
        // Non-object instances of structural nodes are legal for
        // kinds with documented scalar shorthands (scenario
        // strings, string sugars); the parent's form list already
        // vetted the scalar alternative when one exists.
        return;
    };
    let elements = node.elements(Some(value));
    for (key, child) in map {
        // An empty YAML section (`params:` with nothing under
        // it) parses as null; the parser treats it as absent —
        // so does the model.
        if child.is_null() {
            continue;
        }
        match elements.iter().find(|e| e.name == key.as_str()) {
            Some(spec) => {
                validate_against_forms(spec.forms, child, mode, &format!("{path}.{key}"), out);
            }
            None => {
                if node.open_surface().is_none() {
                    out.push(Violation {
                        path: format!("{path}.{key}"),
                        message: format!("unknown element on closed node kind '{}'", node.kind()),
                    });
                }
            }
        }
    }
    if mode == Mode::Complete {
        for spec in &elements {
            let satisfied =
                map.contains_key(spec.name) || spec.aliases.iter().any(|a| map.contains_key(*a));
            if spec.required && !satisfied {
                out.push(Violation {
                    path: path.to_string(),
                    message: format!(
                        "required element '{}' missing on '{}'",
                        spec.name,
                        node.kind()
                    ),
                });
            }
        }
    }
}

/// Validate an entire (possibly partial) workload AST against
/// the construction grammar.
pub fn validate_workload(ast: &Value, mode: Mode) -> Vec<Violation> {
    let mut out = Vec::new();
    validate_node(&WORKLOAD, ast, mode, "workload", &mut out);
    out
}

// ─── Discovery over a partial AST ─────────────────────────────────

/// Resolve the node kind at `path` inside `partial` (a possibly
/// incomplete workload AST) and enumerate its valid sub-elements
/// there. Path segments are the YAML keys / author-chosen names
/// from the root; list indices are decimal. Returns `None` when
/// the path leaves the modeled grammar (e.g. descends into a
/// declared-open surface).
pub fn discover_at<'v>(
    partial: &'v Value,
    path: &[&str],
) -> Option<(&'static dyn EnumerableNode, Vec<ElementSpec>)> {
    let mut node: &'static dyn EnumerableNode = root();
    let mut value: Option<&'v Value> = Some(partial);

    let mut segs = path.iter().peekable();
    while let Some(seg) = segs.next() {
        let elements = node.elements(value);
        let spec = elements.iter().find(|e| e.name == *seg)?;
        let child_value = value.and_then(|v| v.get(*seg));
        let mut next: Option<&'static dyn EnumerableNode> = None;
        let mut named_map = false;
        let mut list = false;
        for f in spec.forms {
            // When the partial AST already shows the child's
            // shape, the matching structural form wins; otherwise
            // the first structural form declared.
            match f {
                Form::Node(n) => {
                    next = Some(*n);
                }
                Form::NamedMap(n) => {
                    if next.is_none() || matches!(child_value, Some(v) if v.is_object()) {
                        next = Some(*n);
                        named_map = true;
                        list = false;
                    }
                }
                Form::ListOf(n) => {
                    if next.is_none() || matches!(child_value, Some(v) if v.is_array()) {
                        next = Some(*n);
                        list = true;
                        named_map = false;
                    }
                }
                _ => {}
            }
        }
        node = next?;
        if named_map || list {
            match segs.next() {
                Some(name) => {
                    value = child_value.and_then(|v| {
                        if list {
                            name.parse::<usize>().ok().and_then(|i| v.get(i))
                        } else {
                            v.get(*name)
                        }
                    });
                }
                None => {
                    return Some((node, node.elements(None)));
                }
            }
        } else {
            value = child_value;
        }
    }
    let elements = node.elements(value);
    Some((node, elements))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(specs: &[ElementSpec]) -> Vec<&'static str> {
        specs.iter().map(|e| e.name).collect()
    }

    /// Serde field probe: capture the exact static `fields` list
    /// the derive passes to `deserialize_struct`, so a table can
    /// be pinned against the MODEL TYPE itself.
    fn fields_of<'de, T: serde::Deserialize<'de>>() -> Option<&'static [&'static str]> {
        use std::cell::Cell;
        struct Probe<'a>(&'a Cell<Option<&'static [&'static str]>>);
        impl<'de, 'a> serde::Deserializer<'de> for Probe<'a> {
            type Error = serde::de::value::Error;
            fn deserialize_struct<V>(
                self,
                _name: &'static str,
                fields: &'static [&'static str],
                _visitor: V,
            ) -> Result<V::Value, Self::Error>
            where
                V: serde::de::Visitor<'de>,
            {
                self.0.set(Some(fields));
                Err(serde::de::Error::custom("field probe"))
            }
            fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
            where
                V: serde::de::Visitor<'de>,
            {
                Err(serde::de::Error::custom("field probe"))
            }
            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64
                char str string bytes byte_buf option unit unit_struct
                newtype_struct seq tuple tuple_struct map enum
                identifier ignored_any
            }
        }
        let cell = Cell::new(None);
        let _ = T::deserialize(Probe(&cell));
        cell.get()
    }

    /// The phase table is pinned to `WorkloadPhase` ITSELF (one
    /// documented nesting exception: `tries_backoff` rides
    /// inside `tries:`'s map form).
    #[test]
    fn phase_table_matches_the_model_struct() {
        let probed =
            fields_of::<crate::model::WorkloadPhase>().expect("WorkloadPhase is a serde struct");
        let mut probed: Vec<&str> = probed.to_vec();
        probed.retain(|f| *f != "tries_backoff");
        probed.sort_unstable();
        let mut table = names(PHASE_ELEMENTS);
        table.sort_unstable();
        assert_eq!(
            probed, table,
            "PHASE_ELEMENTS drifted from the WorkloadPhase struct — \
             update the spec table (and its forms/docs) in lock-step"
        );
    }

    /// Stop-condition and continue-if tables are pinned to their
    /// serde structs the same way (aliases are extra table
    /// entries by design).
    #[test]
    fn serde_backed_tables_match_their_structs() {
        let probed = fields_of::<crate::model::StopConditionSpec>()
            .expect("StopConditionSpec is a serde struct");
        for f in probed {
            assert!(
                STOP_WHEN_ELEMENTS.iter().any(|e| e.name == *f),
                "STOP_WHEN_ELEMENTS missing struct field '{f}'"
            );
        }
        let probed =
            fields_of::<crate::model::ContinueIfSpec>().expect("ContinueIfSpec is a serde struct");
        for f in probed {
            assert!(
                CONTINUE_IF_ELEMENTS.iter().any(|e| e.name == *f),
                "CONTINUE_IF_ELEMENTS missing struct field '{f}'"
            );
        }
    }

    #[test]
    fn tables_are_duplicate_free_and_nodes_registered() {
        let mut kinds = std::collections::BTreeSet::new();
        for n in ALL_NODES {
            assert!(kinds.insert(n.kind()), "duplicate node kind '{}'", n.kind());
        }
        for n in ALL_NODES {
            let mut seen = std::collections::BTreeSet::new();
            for e in n.elements(None) {
                assert!(
                    seen.insert(e.name),
                    "node '{}': duplicate element '{}'",
                    n.kind(),
                    e.name
                );
                // Every node referenced from a form is itself in
                // ALL_NODES — the enumeration surface is closed
                // over the reference graph (references are direct,
                // so this cannot dangle; it can only be missing
                // from the tooling list).
                for f in e.forms {
                    if let Form::Node(c) | Form::NamedMap(c) | Form::ListOf(c) = f {
                        assert!(
                            ALL_NODES.iter().any(|k| k.kind() == c.kind()),
                            "kind '{}' not in ALL_NODES",
                            c.kind()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn discovery_walks_a_partial_ast_with_narrowing() {
        let partial: Value = serde_yaml::from_str(
            r#"
phases:
  load:
    poll:
      until: "x == 1"
    ops:
      probe:
        stmt: "S"
"#,
        )
        .expect("yaml");

        let (node, elements) = discover_at(&partial, &[]).expect("root");
        assert_eq!(node.kind(), "workload");
        assert!(elements.iter().any(|e| e.name == "phases"));

        let (node, elements) = discover_at(&partial, &["phases", "load"]).expect("phase");
        assert_eq!(node.kind(), "phase");
        let conc = elements
            .iter()
            .find(|e| e.name == "concurrency")
            .expect("concurrency");
        assert_eq!(conc.forms, &[Form::Vocab(&["1"])]);
        assert!(
            !elements.iter().any(|e| e.name == "tags"),
            "inline ops exclude the tags selector"
        );

        let (node, elements) = discover_at(&partial, &["phases", "load", "poll"]).expect("poll");
        assert_eq!(node.kind(), "poll");
        assert!(elements.iter().any(|e| e.name == "until" && e.required));

        let (node, elements) =
            discover_at(&partial, &["phases", "load", "ops", "probe"]).expect("op");
        assert_eq!(node.kind(), "op");
        assert!(elements.iter().any(|e| e.name == "stmt"));

        let (node, _) = discover_at(&partial, &["phases"]).expect("phase kind");
        assert_eq!(node.kind(), "phase");

        assert!(discover_at(&partial, &["phases", "load", "nope", "deeper"]).is_none());
    }

    #[test]
    fn abstract_ops_drop_the_statement_payload() {
        let partial: Value = serde_yaml::from_str(
            r#"
phases:
  p:
    ops:
      slot:
        abstract:
          needs:
            q: vec_f32
"#,
        )
        .expect("yaml");
        let (_, elements) = discover_at(&partial, &["phases", "p", "ops", "slot"]).expect("op");
        assert!(
            !elements.iter().any(|e| e.name == "stmt"),
            "abstract slots carry no statement payload"
        );
        assert!(elements.iter().any(|e| e.name == "abstract"));
    }

    #[test]
    fn validation_accepts_a_well_formed_document() {
        let ast: Value = serde_yaml::from_str(
            r#"
description: ok
params:
  p0: "3"
phases:
  load:
    cycles: 5
    concurrency: 2
    bindings: |
      const c := 1
    ops:
      ins:
        stmt: "X {p0}"
        capture:
          w: /0/v
scenarios:
  default:
    - load
"#,
        )
        .expect("yaml");
        let violations = validate_workload(&ast, Mode::Complete);
        assert!(
            violations.is_empty(),
            "unexpected violations: {violations:#?}"
        );
    }

    #[test]
    fn validation_flags_bad_forms_and_missing_required() {
        let ast: Value = serde_yaml::from_str(
            r#"
phases:
  load:
    rate: "not-a-number"
    checkpoint: "bogus-mode"
    poll:
      interval_ms: 50
    metrics:
      m0:
        kind: "sideways"
"#,
        )
        .expect("yaml");
        let violations = validate_workload(&ast, Mode::Complete);
        let paths: Vec<&str> = violations.iter().map(|v| v.path.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.ends_with("rate")),
            "rate form violation expected: {violations:#?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("checkpoint")),
            "checkpoint vocab violation expected"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("kind")),
            "metric kind vocab violation expected"
        );
        // poll.until is required and missing under Complete.
        assert!(
            violations.iter().any(|v| v.message.contains("'until'")),
            "poll.until required-missing expected: {violations:#?}"
        );
        // Partial mode: same doc, no required-missing findings.
        let partial = validate_workload(&ast, Mode::Partial);
        assert!(
            !partial.iter().any(|v| v.message.contains("required")),
            "partial mode must not demand required elements"
        );
    }
}
