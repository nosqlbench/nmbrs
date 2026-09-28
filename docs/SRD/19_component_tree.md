# 19: Component Tree and Metric Scoping

How hierarchical execution context provides dimensional labels
for metrics, and how metric lifecycle is bounded by component
scope.

---

## Problem

The cql_vector workload runs a search phase twice per profile:
once immediately after rampup (before index compaction), and
once after awaiting index completion. Both produce recall@k
metrics. Without distinct labeling, the results are ambiguous —
which recall came from which execution context?

More generally: any nested control flow (for_each, do_until)
creates interior scopes where metrics are produced. These
metrics need dimensional labels that capture WHERE in the
execution tree they were generated, not just WHAT they measure.

---

## Design: Component Tree

Every execution scope is a node in a component tree. The tree
mirrors the scenario execution structure:

```
Session
  └── Scenario "default"
        ├── Phase "discover"
        ├── Phase "create_keyspace"
        └── ForEach profile=label_01
              ├── Phase "drop_table"
              ├── Phase "create_table"
              ├── Phase "create_index"
              ├── Phase "rampup"
              ├── Phase "search_pre_index" [label: stage=pre]
              ├── Phase "await_index"
              └── ForEach k=100
                    └── Phase "search" [label: stage=post]
```

Each node carries:
- **Its own labels**: key=value pairs that describe this node
- **Inherited labels**: accumulated from all ancestors
- **Full label set**: own + inherited, used for all metrics
  created within this scope

---

## Label Accumulation

Labels compose downward through the tree. A metric created at
any node carries the full label chain from root to that node:

```
Session:   session="20260415_abc"
  ForEach: profile="label_01"
    Phase: phase="search", stage="post", k="100"

→ metric labels: {session="20260415_abc", profile="label_01", phase="search", stage="post", k="100"}
```

This is automatic — the executor pushes labels onto a stack
as it enters each scope, and metrics created within that scope
inherit the full stack.

### Workload-Declared Labels

Control flow nodes can declare labels in the YAML:

```yaml
- for_each: "profile in matching_profiles(...)"
  label: profile    # use the iteration variable as a label
  phases:
    - search_before_index:
        label: stage=pre
    - await_index
    - search_after_index:
        label: stage=post
```

When `label:` is not specified on a for_each, the iteration
variable name is used as the label key by default.

### Phase Names as Labels

Every phase automatically contributes `phase="{name}"` to the
label set. This is implicit — no declaration needed.

---

## Label Ownership Invariant

**Each dimensional label is a condensed `(semantic, instance)`
pair: the KEY names the kind, the VALUE names the instance.**
`session="20260415_abc"` reads as "this is a *session*, and its
id is `20260415_abc`"; `exec_id="2"` as "an *execution*, id 2";
`phase="search"` as "a *phase* named search". The key already
carries the type — so a separate `type=` label would be
redundant, and there is none.

From this follows the **label-ownership invariant**:

> A label name is owned by exactly one component in any ancestor
> chain. Once a name is set on a component at initialization, no
> descendant may redeclare it — neither with a differing value
> (which would silently corrupt the dimensional cell) nor with
> the same value (which makes ownership ambiguous). Each
> component declares ONLY the labels it introduces; ancestors'
> labels are inherited, never restated.

Each tier therefore owns a disjoint slice of the label set:

| Tier | Owns | Cardinality |
|---|---|---|
| **Session** | `session` | one per process (SRD-45) |
| **Execution** | `exec_id`, `workload` | 1..N per session (SRD-77 / SRD-88) |
| **Scenario / for_each / phase** | `phase`, iteration vars (`profile`, `k`, …), explicit `label:` | per scope node |
| **Op / dispenser** | `op` | per op template |

```
Session     session="20260415_abc"
  └─ Execution  exec_id="1", workload="cql_vector"
        └─ ForEach  profile="label_01"
              └─ Phase  phase="search", stage="post", k="100"
                    └─ Op  op="ann_query"
```

A metric created at the Op node still carries the *full*
composed set `{session, exec_id, workload, profile, phase,
stage, k, op}` — the tree recomposes it from the ancestor chain
— but every name appears exactly once and is contributed by its
owning tier. [`component::attach`](../../crates/nmbrs-metrics/src/component.rs)
**enforces** this at attach time: a child whose own labels
collide with an ancestor's is a construction bug and panics,
rather than letting the composition silently pick a winner. See
[SRD-88](88_concurrent_executions.md) for why the Session and
Execution tiers are split (concurrent executions share one
session component and each own their `exec_id` child).

---

## Metric Lifecycle

Metrics are scoped to the component that creates them. When
the component's execution completes, its metrics are:
1. Captured to the SQLite reporter (final snapshot)
2. Available for summary reporting
3. Disassociated from the active tree (no leak)

This means:
- Metrics from "search (stage=pre)" and "search (stage=post)"
  are distinct instances with different label sets
- Each for_each iteration produces its own metric instances
- Aggregation across iterations is done at query time via
  the SQLite dimensional labels

---

## Implementation

### Label Stack in ExecCtx

The executor context carries a label stack that grows as the
tree is traversed:

```rust
struct ExecCtx {
    // ... existing fields ...
    label_stack: Vec<(String, String)>,  // accumulated labels
}
```

`execute_tree` pushes labels on entry, pops on exit:

```rust
ScenarioNode::ForEach { spec, children, .. } => {
    for value in values {
        ctx.push_label(&var, &value);
        execute_tree(ctx, children, &inner).await?;
        ctx.pop_label();
    }
}
```

### Labels → phase component

When `run_phase` creates a phase component, it declares ONLY the
labels owned at that tier — the live label stack (`for_each`
levels + `phase`), via `ctx.incremental_labels()`. It does NOT
restate `{session, exec_id, workload}`; those are owned by the
session + execution ancestors and composed in by `attach`:

```rust
ctx.push_label("phase", phase_name);
let phase_own_labels = ctx.incremental_labels(); // for_each + phase only
ctx.pop_label();
let phase_component = Component::new(phase_own_labels, …);
component::attach(&ctx.session_component, &phase_component);
// phase_component.effective_labels() now == {session, exec_id,
// workload} (inherited) + {…for_each, phase} (own)
```

`ctx.labels()` still returns the *full* composed set for callers
that need it directly (e.g. matching metric instances for a
resume purge), but a component's OWN labels are always the
incremental slice — never the full set (label-ownership
invariant, above).

### Labels → Metrics

ActivityMetrics, ValidationMetrics, and all instruments receive
the composed Labels at creation time. The SQLite reporter
stores the full label set in its normalized schema and the
denormalized `spec` column.

### Labels → Summary

The summary report groups by label dimensions. With the full
label set, queries like:

```sql
SELECT mi.spec, sv.mean FROM sample_value sv
JOIN metric_instance mi ON sv.instance_id = mi.id
WHERE mi.spec LIKE 'recall%'
ORDER BY mi.spec;
```

naturally separate pre-index and post-index results because
`stage=pre` vs `stage=post` appears in the spec.

---

## Interaction with Existing Features

### Polydat Scope Composition

The label stack parallels the Polydat scope stack. Each for_each
level that creates a Polydat scope also pushes a label. The GK
scope handles variable values; the label stack handles metric
identity. They are orthogonal but aligned.

### for_each Variables as Labels

By default, for_each iteration variables become labels
automatically. `for_each: "profile in ..."` pushes
`profile="{value}"` onto the label stack. This is the common
case — no explicit `label:` needed.

### do_while / do_until

Loop constructs push their counter variable as a label if
declared: `do_until: "empty" counter: attempt` pushes
`attempt="{i}"` per iteration. Without a counter, the loop
contributes no label (iterations are anonymous).

### Diagnostic (dryrun=wiring)

The wiring/value-provenance output includes the full label stack
context for each phase, showing where in the tree the analysis
applies.

---

## YAML Syntax

### Explicit labels on phases

```yaml
phases:
  search_pre:
    label: stage=pre
    # ... ops ...
  search_post:
    label: stage=post
    # ... ops ...
```

### Explicit labels on control flow nodes

```yaml
- for_each: "profile in ..."
  label: profile
  phases:
    - rampup
    - for_each: "k in {k_values}"
      label: k
      phases:
        - search
```

### No explicit label (default)

for_each uses its variable name. Phases use their phase name.
do_while/do_until use their counter name (if declared).

---

## Migration

The current system uses `activity="phase_name (var=value)"`
as a flat string. This is replaced by structured labels:
`phase="phase_name", profile="label_01", k="100"`. The
activity name string is still constructed for display, but
metrics use the structured labels for dimensional analysis.

No breaking change — the SQLite schema already supports
arbitrary label sets. The `spec` column gains richer labels
automatically.
