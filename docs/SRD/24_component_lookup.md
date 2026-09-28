# 24: Component Lookup by Label Predicate

> **Status.** Design committed and shipping in code
> (`crates/nmbrs-metrics/src/selector.rs`: `Selector` grammar, glob
> patterns, and the component-tree lookup helpers
> `find` / `find_one` / `any` / `count`). Consumed by SRD 23's
> `ControlTarget` and by the metrics query layer.

The runtime component tree (SRD 19) carries dimensional labels
on every node. Today code that needs to address a component
does so either by direct `Arc<RwLock<Component>>` reference
(wire-through from whoever created it) or by walking the tree
and matching on one field at a time. Dynamic controls (SRD 23)
need a cleaner story: **a single canonical facility for
finding components by full-or-partial label predicates**, used
by the controls API, `MetricsQuery` selection, scripted
orchestration, and the future web API.

North stars:

- **Dimensional labels are the identity.** A component is
  addressed by `type=phase, name=rampup, profile=label_00` —
  never by an opaque path string, and never by in-process
  reference outside the owning crate.
- **One predicate grammar, many consumers.** The same
  predicate form should work for controls, metric selection,
  structural queries ("how many running phases?"), and any
  future caller.
- **Full and partial matching.** A caller that knows every
  label's exact value gets an exact single match. A caller
  with wildcards (`phase=*`, `profile=label_*`) gets a set.
  A caller with no label constraints matches every component
  in scope.
- **Scope-aware.** Queries can run from the session root (the
  whole tree) or from any sub-component (its subtree). A
  phase-local query never reaches beyond the phase.
- **Resolver, not a copy.** Lookup returns live references
  (`Arc<RwLock<Component>>`), not snapshots — callers that
  want a point-in-time value take one explicitly.

---

## Predicate grammar

A **component selector** is a conjunction of label clauses. Each
clause is `key <op> value`:

| Operator | Meaning | Example |
|---|---|---|
| `=` | exact match | `phase=rampup` |
| `!=` | exact non-match | `phase!=teardown` |
| `~=` | glob / wildcard match against the value | `profile~=label_*` |
| `?` | label must be present (any value) | `profile?` |
| `!?` | label must be absent | `profile!?` |

Multiple clauses combine with AND. No OR, no nesting — a caller
that needs the union of two selectors issues two queries and
unions the results. Keeps the grammar small, the parser easy,
and the evaluation fast against each candidate's `Labels`.

**Text form** (used by CLI, YAML, REST paths, and Polydat nodes):

```
type=phase,name=rampup,profile~=label_*,optimize_for=RECALL
```

**In-code builder form** (used by Rust call sites):

```rust
let sel = Selector::new()
    .eq("type", "phase")
    .eq("name", "rampup")
    .glob("profile", "label_*")
    .eq("optimize_for", "RECALL");
```

Both resolve to the same intermediate representation. Text
form is parsed via a small `ParamsParser`-style grammar; the
builder is the zero-allocation path used by the hot runtime.

---

## Matching rules

A candidate component matches a selector iff **every clause
evaluates true** against the candidate's `effective_labels`
(SRD 19) — the merged labels from the candidate and every
ancestor in the tree. This is the same label set the metrics
system already uses for series identity, so a selector that
matches a metric's labels also matches the component that
emitted it. No divergence.

Clause evaluation:

- `key=value` — candidate has `key` AND its value equals `value`.
- `key!=value` — candidate has `key` AND its value != `value`,
  OR the candidate does not have `key` at all. Absence is a
  non-match, not an error.
- `key~=glob` — candidate has `key` AND the value matches the
  glob (`*`, `?` metacharacters; no regex).
- `key?` — candidate has `key` (any value).
- `key!?` — candidate does not have `key`.

The "present but absent" edge cases are called out deliberately:
Rust's `Labels::get(key)` returns `Option<&str>`, and a
selector's correctness depends on distinguishing "absent" from
"empty-string".

---

## API surface

### `Selector`

```rust
pub struct Selector {
    clauses: Vec<Clause>,
}

impl Selector {
    pub fn new() -> Self;
    pub fn parse(s: &str) -> Result<Self, SelectorParseError>;
    pub fn eq(self, key: &str, value: &str) -> Self;
    pub fn ne(self, key: &str, value: &str) -> Self;
    pub fn glob(self, key: &str, pat: &str) -> Self;
    pub fn present(self, key: &str) -> Self;
    pub fn absent(self, key: &str) -> Self;
    pub fn matches(&self, labels: &Labels) -> bool;
}
```

### Lookup from the tree

The component type grows query methods. A query run from a
given component scopes to that subtree (inclusive).

```rust
impl Component {
    /// Every component in this subtree matching the selector.
    /// Order is pre-order DFS — parents before children, children
    /// in insertion order.
    pub fn find(&self, sel: &Selector) -> Vec<Arc<RwLock<Component>>>;

    /// The single component matching the selector. `Err` if zero
    /// or more than one match — callers that want "any one"
    /// use `find` and take the first.
    pub fn find_one(&self, sel: &Selector)
        -> Result<Arc<RwLock<Component>>, LookupError>;

    /// Does any component match? Short-circuits on first hit.
    pub fn any(&self, sel: &Selector) -> bool;

    /// Count of matches. O(N) over the subtree.
    pub fn count(&self, sel: &Selector) -> usize;
}

pub enum LookupError {
    NotFound,
    Ambiguous { count: usize },
}
```

### Ergonomic shortcuts

A tiny macro keeps call sites readable:

```rust
let ann_queries = session.root.find(
    selector!(type = "phase", name = "ann_query")
);
```

The macro emits a `Selector::new().eq(...).eq(...)` chain at
compile time. No runtime string parsing on the hot path.

---

## Wildcards and globs

Glob matching uses the common `fnmatch`-style grammar:

- `*` — matches any sequence of characters (including empty).
- `?` — matches exactly one character.
- everything else matches literally.

No character classes, no escapes, no regex. If a workload
needs more expressive matching it's a signal to promote the
intent to a proper label rather than over-grow the selector
language.

---

## Use cases

- **Dynamic controls (SRD 23):** `ControlTarget = (Selector,
  control_name)`. A write resolves the selector → one
  component → the control registry on that component.
- **Metrics selection (SRD 42):** `MetricsQuery::now(selection)`
  and friends already take a label-style selection; consolidate
  under this `Selector` type and call the same machinery from
  both sides.
- **Component-tree structural queries:** "How many phases of a
  given name / profile?" are pure-label questions —
  `session.count(selector!(type = "phase", name = "rampup"))`.
  Questions that additionally depend on lifecycle state ("how
  many *running* phases?") combine a selector hit with
  `Component::state()` — the selector narrows by labels, the
  lifecycle check filters the matches. Lifecycle isn't part
  of the selector grammar (see "Non-goals (continued)" below).
- **`dryrun=controls`:** Enumerate every component in the tree,
  dump its declared controls with current value + type, so
  users know what's mutable before trying to set it.
- **Web API routing:** `POST /controls?sel=type=phase,name=rampup`
  parses the query string directly into a `Selector`.
- **Polydat scripting:** Control-targeting Polydat nodes
  (`control_set(selector, name, value)`) accept text-form
  selectors so scenarios can write against other phases.

---

## Non-goals

- **Regex matching.** Globs only. Scenarios that want
  anchored regex are almost certainly missing a label.
- **Ordering / sorting.** Pre-order DFS is fine for
  "enumerate everything matching". Callers that need a
  specific order sort the returned Vec themselves.
- **Cross-session queries.** Every `Selector` runs within
  one session's component tree. The summary-report pipeline
  queries SQLite, not the live tree.
- **Historical queries.** The selector matches the component
  tree as it exists now. Past states are not recoverable
  without the cadence reporter / SQLite round-trip.

---

## Crate placement

`Selector`, its text-form parser, and the `selector!` macro
live with `Component` — in `nmbrs-metrics::component`'s scope
(the same crate and module space `Component` itself occupies).
Every downstream consumer already depends on `nmbrs-metrics` for
`Labels`; none of them need a new crate. Metrics selection
(SRD 42) consolidates onto the same `Selector` type from
inside `nmbrs-metrics` without any circular-dependency concerns.

---

## Non-goals (continued)

Predicates over *component-instance* state (e.g. running /
stopped lifecycle) are outside the scope of this selector.
`Selector` matches on **labels only** — the immutable-for-
lookup dimensional identity of a component. If a query needs
to filter on lifecycle state the caller combines a selector
hit with a code-level `Component::state()` check, or a
dedicated API is added when the need arises. That keeps the
selector grammar small and the semantics unambiguous:
"labels in, components matching those labels out".

---

## Open questions

*(none outstanding — the state-as-label, label-set
normalization, and crate-placement questions are settled
above; label-set hashing is an implementation detail tracked
with the implementation itself.)*
