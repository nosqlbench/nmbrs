# SRD-66 — Runtime Feature Detection: result-wire polydat-call form, shared-wire upward writes, and `pick` for branched dispatch

**Status:** Pushes 1+3 shipped; Push 2 partial as of
2026-05-08. The three stdlib node functions (`pick`,
`exactly_one_value`, `log_debug`/`log_info`/`log_warn`/
`log_error`) are landed and exercised by tests. The vari-
structured `result:` field's data model + parser (string /
list / map shapes) is landed; the result-wire dispatcher
keeps `count` / `ok` / path-expr semantics through the new
type. `OpResult::body.to_text()` is added as the canonical
text projection (default JSON-stringify; per-adapter
overrides for TextBody and others). The
`nmbrs/workloads/cql/full_cql_vector.yaml` migration is
landed in the new shape — `cql_dialect` param removed,
`detect_dialect` phase prepended to every scenario, four
`if`-gated ops collapsed to two `pick`-driven ops. The full
kernel-driven polydat-call path (compile result-bindings into a
runtime-evaluated kernel, write `body` / `count` / `ok`
extern slots per cycle, propagate writes through the
SharedCell upward to outer-scope `shared` wires) is
documented but not yet wired — polydat-call sources currently
emit a Warn at op-init and resolve to their default value,
and the workload won't actually detect the dialect at
runtime until the path lands. See §"Push 2 follow-up:
kernel-driven path" below for the design constraints
discovered during the partial implementation.
**Owner:** runtime / polydat / nmbrs-runtime / workload
authors
**Implementation target:**
  `polydat/src/nodes/pick.rs` (new),
  `nmbrs-runtime/src/wrappers.rs` (result-wire polydat-call dispatch
  — replaces today's deferred-Warn stub at line ~751),
  `nmbrs-runtime/src/fixture.rs` / op-template scope wiring
  (magic `body` extern),
  `nmbrs/workloads/cql/full_cql_vector.yaml` (first consumer)
**Cross-refs:** SRD-12 (GK stdlib), SRD-13d (op-template scope),
  SRD-16 (mutability rules — `shared`), SRD-34 (capture points),
  SRD-40b §5 (result-as-GK), SRD-50 (CQL adapter)

---

## What this SRD enables

Workloads need to **discover facts about the target system at
session bootstrap and route subsequent phases on those facts**.
The motivating concrete case: `full_cql_vector.yaml` today carries
a static `cql_dialect: cass5` workload param that the operator
must hand-set, with every `await_index` op `if`-gated against
that string. The reality is the cluster knows whether it has
`system_views.sai_column_indexes` or `system_views.indexes`, and
a one-shot probe at session start can answer authoritatively.

This SRD gives workloads three composable primitives so the
detect-then-dispatch pattern is expressible end-to-end without
new ad-hoc machinery:

1. **`result:` as a Polydat source block (general-purpose).** The
   `result:` op-template field becomes a multi-line Polydat source
   string — same syntactic shape as `bindings:`, just
   evaluated AFTER the op runs with a small set of pre-bound
   wires injected: `body: Str` (the result body's text
   projection), `count: U64` (row/element count), `ok: Bool`
   (success boolean), plus every name from the op's
   `[name]`-bracket capture declarations. Each `:=`
   assignment in the block declares one result wire. The user
   reads the file and immediately knows it's Polydat source — no
   YAML-mapping-with-magic-source-strings to decode.
   Specialised forms like regex matching, body-length
   thresholds, multi-field arithmetic, and shape predicates
   all compose from existing Polydat stdlib functions
   (`regex_match`, `len`, `if`, etc.) — no per-shape built-in
   needed. **This supersedes SRD-40b §5.1's YAML-mapping
   shape**, which is dropped (no shipped consumer to migrate).
2. **`shared` extern bridge to upward writes** — the existing
   SRD-16 `shared`-modifier wire mechanism (already implemented
   per `polydat/src/kernel/engines.rs::SharedCell`) already
   lets an inner scope's write propagate to an outer scope's
   cell. SRD-66's contribution here is to **document and codify
   the contract for using it from a `result:` wire**, so workload
   authors can rely on it as a normative path.
3. **`pick` node function** — `pick(b0, …, bN-1, v0, …, vN-1) ->
   V`: exactly one of the booleans must be true; returns the
   value at the same index. The branched-dispatch primitive that
   makes "set N booleans, look up the matching value" expressible
   in one Polydat expression.

Together they replace `if: "cql_dialect == 'cass5'"`-style gates
with `pick(has_sai_column_indexes, has_indexes, "...", "...")`
expressions reading detected booleans (set by polydat-call result
wires that ran `regex_match(body, "...")` at the probe phase),
eliminating the static operator-set parameter.

The load-bearing rule:

> **Probe at session start, write detected facts to `shared`
> wires at workload-root scope, branch downstream phases with
> `pick`. No ad-hoc `if:` gates against operator-supplied
> dialect strings.**

---

## Vocabulary

- **Probe phase.** A phase that runs once at scenario bootstrap
  to interrogate the target system. Its ops carry `result:`
  wires that translate response data into typed wire values.
- **Detected fact.** A wire value (boolean, number, string)
  produced by a probe phase and bound at workload-root scope so
  every later phase can read it.
- **Shared wire.** An SRD-16 `shared`-modifier output declared at
  outer (workload-root) scope, materialised as a `SharedCell` so
  inner kernels' writes propagate up to the outer kernel and
  across to sibling inner kernels. This SRD does NOT introduce
  the mechanism; it documents the contract.
- **Result bindings block.** A multi-line Polydat source string
  attached to an op template under `result:` — same syntactic
  shape as `bindings:`, just with a different scope: it's
  evaluated AFTER the op runs, with the body / built-in result
  facts / captures injected as pre-bound wires. Each `:=`
  assignment declares one result wire.
- **Pre-bound result wires.** A small set of wires the runtime
  injects into the result-bindings scope before evaluation:
  `body: Str` (body's text projection), `count: U64`
  (row/element count), `ok: Bool` (success boolean), plus
  every name from the op's existing `[name]`-bracket capture
  declarations.
- **Body projection.** The `body: Str` wire's value — a single
  string derived from the op's `OpResult` body. The default
  projection is `body.to_text()`; adapters can override (e.g.
  CQL exposes the result row's text columns directly without a
  JSON round-trip — see §"Open questions" item 3).
- **`pick` selection.** Single-call branched dispatch: "given N
  booleans and N values, pair-wise return the value of the index
  whose boolean is true." Exactly-one-true is required at eval.

---

## Surface 1 — Result-bindings block (GK source under `result:`)

### Precedent

`result:` is specified by SRD-40b §5.1 with a YAML-mapping
shape: each key is a wire name and its value is a source
string in one of four forms: `count` (row count), `ok`
(success boolean), `<path-expr>` (JSON-path into the result
body), or `<polydat-call>` (arbitrary Polydat expression). The first
three are implemented in
`nmbrs-runtime/src/wrappers.rs::ResultDispenser`. The polydat-call
form emits a one-time Warn ("not yet supported — slot will
resolve to its default") at runtime; the parser at
`wrappers.rs:751` recognises it via a `(` detector but the
eval path is a stub.

**No shipped workload currently uses `result:` in any form.**
The CQL workload uses `metrics:` (SRD-40b §1) but not
`result:`. SRD-66 is the first material consumer of the
mechanism — and the natural moment to **change the syntax
shape** to one that doesn't co-mingle YAML-field assignment
with Polydat wire-write semantics.

### Why the YAML-mapping shape is the wrong syntax

The mapping form has each entry compile differently depending
on the source-string contents — `count` and `ok` are
built-ins, anything containing `(` is a polydat-call, anything else
is a path expression. The shape on the page is the same
(`name: source`), but the meaning slips between assignment of
a literal and a Polydat expression compile. For a workload author
reading the file, the mapping looks like ordinary YAML
key→value pairs, not like wire writes.

`bindings:` already solved this in the workload schema: it's
a multi-line Polydat source string with `:=` assignment. SRD-66
adopts the same shape for `result:`. The fact that `result:`
is Polydat source is then **immediately visible from the
syntax** — no inference required.

### Schema

`result:` is a multi-line scalar containing Polydat source. Each
`<name> := <expr>` assignment declares one result wire. The
expression may reference:

- `body: Str` — the result body's text projection.
- `count: U64` — number of rows / records / elements.
- `ok: Bool` — success boolean.
- Any name from the op's `[name]`-bracket capture declarations.
- Any in-scope wire reachable via the standard scope chain
  (workload-root `shared`, intermediate scopes, the op's own
  `bindings:` block).

The motivating example for SRD-66:

```yaml
ops:
  describe_system_views:
    raw: "describe keyspace system_views"
    result: |
      has_sai_column_indexes := regex_match(body, "(?im)^\s*(VIRTUAL\s+)?TABLE\s+system_views\.sai_column_indexes\s*\(")
      has_indexes            := regex_match(body, "(?im)^\s*(VIRTUAL\s+)?TABLE\s+system_views\.indexes\s*\(")
```

The `|` is YAML's literal-block-scalar marker — every line
preserved verbatim, no escaping of inner double-quotes
needed (mirrors `bindings: |`).

`regex_match` is the existing Polydat stdlib node
(`polydat/src/nodes/regex.rs`).

### Built-in wires available in the result-bindings scope

| Name     | Type   | Source                                                     |
|----------|--------|------------------------------------------------------------|
| `body`   | Str    | `OpResult.body.to_text()` projection (per-adapter override).|
| `count`  | U64    | `OpResult.body.element_count()` — the row / record count.  |
| `ok`     | Bool   | `true` when the inner adapter returned `Ok(_)` or skipped. |
| captures | varies | Names declared via `[name]` brackets in the op's fields.   |

A user-written `body :=` / `count :=` / `ok :=` assignment in
the result-bindings block is a hard error at workload-load
("`<name>` is a runtime-injected wire and cannot be
reassigned in `result:`") — keeps the mental model tight.

### Migration from SRD-40b §5.1's mapping form

The YAML-mapping form is **dropped**. Since no shipped
workload uses it, the migration cost is zero. The four source
forms map to Polydat source as follows:

| SRD-40b mapping form              | SRD-66 Polydat source                        |
|-----------------------------------|------------------------------------------|
| `<name>: count`                   | `<name> := count`                        |
| `<name>: ok`                      | `<name> := ok`                           |
| `<name>: rows[0].field` (path)    | (DEFERRED — see §"Out of scope")         |
| `<name>: <polydat-call>`               | `<name> := <polydat-call>`                    |

Path expressions are deferred because they require structural
access to the body that `body: Str` doesn't provide. They land
when (a) a `body_json: <json-shape>` wire is added alongside
`body`, or (b) a `json_path(body, "rows[0].field")` stdlib
function lands. Both are out of scope for SRD-66; workloads
that need structural extraction stay on the existing `[name]`
bracket-capture machinery (SRD-34).

### Compilation lifecycle

At op-template scope synthesis (SRD-13d Phase 9):

1. The workload loader extracts the `result:` source. If
   empty / absent, no result wires are declared and the
   per-cycle path is the existing no-result fast path.
2. The result-bindings source is compiled as a separate
   scope-extension of the op-template kernel. **Closure
   bindings follow the standard Polydat rule: linkages are made
   only where Polydat module matter detects them.** That is, the
   compiler walks the source's free identifiers and resolves
   each one. References that hit a runtime-injected name
   (`body`, `count`, `ok`, or any name from the op's
   `[name]`-bracket captures) bind to the corresponding
   extern slot. References to outer-scope wires resolve via
   `bind_outer_scope`. Names with no binding are unbound-
   identifier compile errors.
3. The runtime instantiates extern slots only for names the
   source actually references — no pre-declaration of unused
   `body` / `count` / `ok` slots. This is the same closure-
   binding economy `bindings:` already uses; result bindings
   inherit the rule unchanged.
4. Compile-time errors (unknown function, type mismatch,
   reassignment of a runtime-injected name) are reported
   with the op-template name and the offending line in the
   result-bindings source.
5. The output names of this scope (every left-hand-side
   identifier in `:=` assignments and every map-shape key)
   become the op's result wires — each materialised as an
   output of the op's main GkState so subsequent ops, metric
   wrappers, and outer-scope `shared` cells can read them.
   For map shape, an additional composite-map wire is
   declared (see Surface 1 §"Shape 3").

Per cycle:

1. Inner adapter executes; produces `OpResult { body, captures }`.
2. Runtime writes the body projection to the kernel's `body`
   input slot, **iff the slot exists** (i.e. the source
   referenced `body`).
3. Runtime writes `count` and `ok` from the OpResult, again
   only when those slots exist.
4. Captures are written via the existing `fiber.capture(name,
   value)` path — only the slots that exist.
5. The kernel re-evaluates dependent nodes (provenance-driven).
6. The result-wire dispatcher reads each declared output and
   writes it through to the op's main GkState (same
   `set_input` path the existing `count`/`ok` built-ins use,
   so shared-cell propagation works uniformly). For map
   shape, the dispatcher additionally assembles the
   composite-map wire from each entry's typed value.

### Why this is a self-consistent surface

- The `result:` block looks like Polydat source because it IS GK
  source. No special parser; no per-shape detector.
- Existing Polydat stdlib functions (`regex_match`, `len`, `if`,
  `eq`, `and`, `hash_str`, …) compose against the pre-injected
  `body` / `count` / `ok` / capture wires automatically.
- The pre-injected wires are rare and named clearly; a workload
  author scanning a file sees `regex_match(body, …)` and
  immediately knows `body` is a runtime wire, not an
  adapter-specific keyword.
- The whole thing is one mental model: `bindings:` and
  `result:` are both Polydat source blocks, differing only in WHEN
  they evaluate (init-time vs. post-op) and WHICH wires they
  see.

### Strict-mode interactions

Per SRD-15, the following promote from warn → error under
`--strict`:

- A result-bindings expression that references `body` on an op
  whose adapter exposes only an empty body (e.g. an `stdout`
  op). This is a "pattern would always match against empty
  string" trap; warn under normal mode, error under strict.
- A result-bindings assignment whose right-hand side compiles
  to a constant (no dependency on `body`, `count`, `ok`,
  captures, or any in-scope wire). The user clearly meant to
  read SOMETHING from the result; a constant binding belongs
  in `bindings:`, not `result:`.

Always-error, strict-independent:

- Source string fails to compile (unknown function, type
  mismatch, unbound identifier). The diagnostic names the
  result wire and the offending source line.
- Reassignment of a pre-injected wire (`body :=`, `count :=`,
  `ok :=`).
- An identifier on the left of `:=` that doesn't match the GK
  identifier grammar (`[a-zA-Z_][a-zA-Z0-9_]*`).

### Acceptance

- A workload declaring
  ```yaml
  result: |
    started_with_x := regex_match(body, "^x")
  ```
  compiles cleanly; the wire resolves to `Bool(true)` when
  the body's text projection starts with `x` and `Bool(false)`
  otherwise.
- A workload declaring
  ```yaml
  result: |
    rows         := count
    has_results  := count > 0
    schema_size  := len(body)
  ```
  evaluates each wire correctly.
- A result-bindings expression that uses an undeclared
  identifier errors at workload-load with the wire name +
  identifier named.
- The eval cost per cycle is the cost of the dependent nodes
  re-evaluating with `body` / `count` / `ok` / captures
  changed; the existing Polydat provenance machinery skips
  unchanged subgraphs.

---

## Surface 2 — `shared` wire upward writes from a `result:` wire

### What's already shipped

The `shared` modifier on outputs (SRD-16) is implemented end-
to-end in `polydat`. The cell-backing primitive lives at
`polydat/src/kernel/engines.rs::SharedCell` (an
`Arc<Mutex<Value>>`); `bind_outer_scope` plumbs the outer's
`shared` output through to the inner's matching `extern` slot
via the same cell, so inner-side writes propagate to outer
reads (and to sibling inner readers) transparently.

What's NOT yet shipped is **the documented contract for using
this from a `result:` wire** — workload authors today have no
written guarantee that "result-wire writes flow up through
`shared`." This SRD provides that guarantee.

### Contract

Given:

- A workload-root `bindings:` block declaring
  `shared <name> := <literal>` (initial value is a literal —
  enforced by the SRD-16 `shared`-init rule).
- An op-template-scope `bindings:` block in a downstream phase
  declaring `extern <name>: <type>`.
- A `result:` block on the same op-template that targets
  `<name>` (string-shape `<name> := <expr>` or map-shape
  `<name>: <source>`).

The result wire's per-cycle write is `state().set_input(slot,
value)` (the same path the existing `count`/`ok` built-ins
take). Because the inner kernel's input slot was bound to the
outer's `SharedCell` at `bind_outer_scope` time, the write
flows to the cell, and the next outer-scope read picks it up.

The contract:

1. **Initial value.** Every `shared` declaration MUST initialise
   to a literal (SRD-16). For a probe-then-detect pattern, the
   initial value is the "not yet probed" default — typically
   `false` for a boolean detection flag.
2. **Single writer per probe phase.** The probe phase that
   discovers a fact is the only writer for that fact's wire.
   Multiple probe phases can each own their own facts; one fact
   should not be set by two probes in the same run.
3. **Last-write-wins.** Concurrent writers serialise at the
   `Mutex`. The SRD-16 semantic (last writer wins by lock
   order) carries through unchanged. Probe phases run with
   `concurrency: 1` (typical) so this is a non-issue in
   practice.
4. **Read-after-write ordering.** A phase that reads a fact
   MUST appear after the phase that writes it in the scenario
   tree's DFS order. The runtime does NOT enforce this
   ordering — workload authors are responsible for sequencing
   probe-then-use scenarios correctly.
5. **Reset behaviour.** A probe phase that re-runs (e.g. via
   resume-then-redo) overwrites the fact. There's no "first
   write wins" guard; the latest probe wins.

### Reading from a downstream phase

A phase that consumes a detected fact declares an `extern` in
its op-template-scope bindings:

```yaml
ann_query:
  bindings: |
    extern has_sai_column_indexes: bool
    extern has_indexes: bool
    target_index_table := pick(has_sai_column_indexes, has_indexes,
                               "system_views.sai_column_indexes",
                               "system_views.indexes")
  ops:
    indexes_present:
      raw: "SELECT index_name FROM {target_index_table} WHERE …"
```

`bind_outer_scope` runs at phase-init time per SRD-13d Phase 9;
the `extern` slot becomes cell-backed and the `target_index_table`
binding evaluates against the live cell value.

### Acceptance

- A workload declaring `shared foo := false` at root and
  `result: |\n  foo := regex_match(body, "...")` in a probe op
  reads `foo == true` (or `false`) in a subsequent phase's
  `extern foo: bool` binding.
- Two sibling phases reading the same `shared` wire after one
  probe phase has set it both see the post-probe value (the
  cross-inner propagation property exercised by
  `shared_two_inners_see_each_others_writes_via_cell` already).
- A workload that declares an `extern foo: bool` without a
  matching workload-root `shared foo := <literal>` errors at
  workload-load with the existing SRD-13d "unbound extern"
  diagnostic.

---

## Surfaces 3–5 — stdlib node functions

The three stdlib node functions that back this SRD —
`pick` (branched dispatch), `exactly_one_value`
(structured-body unwrap), and `log_debug` / `log_info` /
`log_warn` / `log_error` (in-binding diagnostics) — are
polydat-substrate and have moved into the polydat crate:

- [polydat/docs/design/library_catalog.md §"Diagnostic" and §"Branched dispatch and structured-body assertions"](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/library_catalog.md)
  — hoisted 2026-05-30 into the unified library catalog
  (see [docs/polydat_srd_audit.md](../polydat_srd_audit.md))

The workload-author surface (Surfaces 1 and 2 above), the
worked migration example, push order, decisions, open
questions, and the Push 2 follow-up (kernel-driven polydat-call
slot contract) remain in this file.

---

## Worked example — `full_cql_vector.yaml` migration

### Today

```yaml
params:
  cql_dialect: cass5

phases:
  await_index:
    ops:
      indexes_present_cass5:
        if: "cql_dialect == 'cass5'"
        raw: "SELECT … FROM system_views.sai_column_indexes WHERE …"
      indexes_built_cass5:
        if: "cql_dialect == 'cass5'"
        raw: "SELECT … FROM system_views.sai_column_indexes WHERE …"
        poll: await_empty
      indexes_present_vendor:
        if: "cql_dialect == 'vendor'"
        raw: "SELECT … FROM system_views.indexes WHERE …"
      indexes_built_vendor:
        if: "cql_dialect == 'vendor'"
        raw: "SELECT … FROM system_views.indexes WHERE …"
        poll: await_empty
```

Four ops; two pairs of duplicated SQL differing only on table
name; an operator-supplied dialect string the runtime never
verifies.

### Under SRD-66

```yaml
bindings: |
  shared has_sai_column_indexes := false
  shared has_indexes := false

scenarios:
  fulltest:
    - phase: detect_dialect          # NEW — once at scenario start
    - scenario: test_oracles
    - scenario: test_fknn

phases:
  detect_dialect:
    concurrency: 1
    bindings: |
      extern has_sai_column_indexes: bool
      extern has_indexes: bool
    ops:
      describe_system_views:
        raw: "DESCRIBE KEYSPACE system_views"
        result: |
          schema_text            := exactly_one_value(body)
          has_sai_column_indexes := log_info(regex_match(schema_text, "(?im)^\s*(VIRTUAL\s+)?TABLE\s+system_views\.sai_column_indexes\s*\("))
          has_indexes            := log_info(regex_match(schema_text, "(?im)^\s*(VIRTUAL\s+)?TABLE\s+system_views\.indexes\s*\("))

  await_index:
    concurrency: 1
    bindings: |
      extern has_sai_column_indexes: bool
      extern has_indexes: bool
      target_index_table := pick(
        has_sai_column_indexes, has_indexes,
        "system_views.sai_column_indexes",
        "system_views.indexes")
    ops:
      indexes_present:
        raw: |
          SELECT index_name FROM {target_index_table}
          WHERE keyspace_name = '{keyspace}'
            AND table_name = '{table}'
            AND index_name = '{table}{vector_idx_suffix}'
          ALLOW FILTERING
        verify:
          - min_rows: 1
        strict: true
      indexes_built:
        raw: |
          SELECT index_name FROM {target_index_table}
          WHERE keyspace_name = '{keyspace}'
            AND table_name = '{table}'
            AND index_name = '{table}{vector_idx_suffix}'
            AND is_building = true
          ALLOW FILTERING
        poll: await_empty
        poll_interval_ms: 5000
        timeout_ms: 600000
        poll_max_error_retries: 3
        poll_metric_name: index_build_time_s
```

Two ops instead of four; one detect phase carries the actual
runtime fact; the SQL bodies dedupe; the dialect param is gone.

### Behavioural equivalence

For a Cassandra 5 cluster:
- `describe keyspace system_views` returns text containing
  `VIRTUAL TABLE system_views.sai_column_indexes (`.
- `has_sai_column_indexes` → `true`; `has_indexes` → `false`
  (the sai_column_indexes line, when parsed by the indexes
  pattern, fails on `system_views.s` ≠ `system_views.i`).
- `pick` returns `"system_views.sai_column_indexes"`.
- `await_index`'s ops query the SAI view exactly as before.

For a vendor-build cluster (or any deployment where only
`system_views.indexes` exists):
- `has_sai_column_indexes` → `false`; `has_indexes` → `true`.
- `pick` returns `"system_views.indexes"`.

For an unrecognised cluster (neither table present):
- Both booleans are `false`; `pick` panics with the no-selector
  message. The operator gets a diagnostic naming the missing
  prerequisite, not a confusing "no rows" failure mid-workload.

### What the operator no longer manages

- The `cql_dialect` workload param (gone).
- Per-op `if:` gates against the dialect string (gone).
- The four near-duplicate `indexes_present_*` / `indexes_built_*`
  ops collapse to two (gone).

---

## Migration policy

### Workload migration

The CQL workload's `cql_dialect` param and the `if`-gated ops
get rewritten in one push. Any workload that didn't go through
this exact pattern is unaffected.

### Adding new dialects

A new system whose schema names a different table joins the
`pick` call as one more selector + value pair, with one more
`shared` binding initialised at workload root, and one more
`regex:` line in the probe op's `result:` block. No code
change.

### Backwards compatibility

`pick` and the `regex:` result-wire form are additions — no
existing workload uses these names. The `cql_dialect` param
and `if:` syntax remain valid for workloads that haven't
migrated.

---

## Strict mode

The following promote from warn → error under `--strict` (per
SRD-15):

- A polydat-call result wire that references `body` on an op whose
  adapter exposes only an empty body — see Surface 1
  §"Strict-mode interactions."
- A polydat-call result wire whose compile produces a constant-
  valued expression (no dependency on `body`, captures, or
  in-scope wires) — same section.

Always-error, strict-independent:

- polydat-call source string fails to compile (unknown function,
  type mismatch, unbound identifier).
- `pick` with odd total argument count.
- `pick` with a non-bool selector slot.
- `pick` with mismatched value-list types.
- `extern <name>` without a matching outer-scope `shared
  <name> := <literal>` (existing SRD-13d diagnostic).

---

## Out of scope

- **Probe-phase scheduling primitives.** Workloads sequence
  their probes by placing the detect phase first in a scenario,
  same as any other phase. SRD-66 adds no new
  scheduling/ordering machinery.
- **Adapter-specific body projections.** The default
  `body.to_text()` plus per-adapter overrides (Surface 1)
  cover the SRD-66 use cases. A future generalised "give me
  the body in form X" surface (e.g. `body_json`,
  `body_rows[0]`) can land later as additional magic externs;
  not needed for the motivating use case.
- **`pick`-by-key (`switch`/`match` on string).** A future
  `match(key, "v0", x0, "v1", x1, …)` primitive (SRD memo
  `project_map_intrinsic_deferred`) is a separate function with
  different semantics. SRD-66's `pick` is boolean-selector only.
- **TUI display of detected facts.** Showing the detected
  dialect/feature flags in the status line is a separate
  decision (per SRD-63 readouts); workloads that want this
  declare a status-readout binding referencing the same
  `extern` wires.

---

## Push order

Three pushes with gates:

### Push 1 — Stdlib node functions: `pick`, `exactly_one_value`, `log_*`

- `polydat/src/nodes/pick.rs` implements §"Surface 3".
  Variadic registration; `Arity::VariadicWires { min_wires:
  2 }`; compile-time validators (even arg count, all-bool
  first half, uniform value-half type); eval-time enriched
  panics (no-selector / multi-selector).
- `polydat/src/nodes/exactly_one.rs` implements
  §"Surface 4". One-arg signature; eval-time assertion that
  the structural input has unary shape; enriched panic on
  shape mismatch.
- `polydat/src/nodes/log_levels.rs` implements
  §"Surface 5". Four pass-through node functions
  (`log_debug` / `log_info` / `log_warn` / `log_error`); each
  emits one diag line at its level via SRD-41 and returns
  the input value unchanged.
- Tests: round-trip via DSL, eval cases per each surface's
  Acceptance section.

### Push 2 — `result:` op-template field + result wrapper

- Workload schema:
  - `nmbrs-workload/src/model.rs`: change `ParsedOp.result`
    from `HashMap<String, ResultWireSpec>` to a vari-
    structured `Option<ResultSpec>`, with shapes per
    §"Surface 1 §Schema." Drop the existing
    `ResultWireSpec`; replace with a `ResultSpec` enum
    covering `String(String)` / `List(Vec<ResultSpec>)` /
    `Map(IndexMap<String, ResultMapValue>)`. The parser
    reads any of the three YAML shapes.
  - The old SRD-40b §5.1 YAML-mapping form (with
    `count`/`ok`/`<path-expr>`/`<polydat-call>` source strings)
    is preserved as the **map shape** (§"Surface 1 §Shape
    3") with two refinements: polydat-call magic detection drops
    (any non-short-form string is just GK), and the
    composite-map output is added.
- Op-template scope synthesis (per the closure-bindings rule
  documented at §"Compilation lifecycle"): when `result:` is
  non-empty, the result wrapper's compile pass walks the
  source's free identifiers and wires only the references
  that resolve to runtime-injected values (`body`, `count`,
  `ok`, capture names). Unreferenced runtime values are not
  injected (zero per-cycle cost when not used, matching the
  standard Polydat linkage-detection pattern).
- `OpResult::body` access — Surface 4's `exactly_one_value()`
  takes a structural body; Push 2 settles the Value-enum
  variant for the body wire (see §"Open: body type" in
  Surface 4) and how it round-trips through the JSON-AST
  for map-shape composite output.
- `nmbrs-runtime/src/wrappers.rs::ResultDispenser`:
  - Replaces the count/ok/path-expr/polydat-call source dispatch
    with a kernel-driven path. The wrapper still occupies
    its position in the SRD-32a registry with
    `requires_inner: [TRAVERSE]` — unchanged.
  - Per cycle: write `body` / `count` / `ok` / captures to
    the kernel's input slots (only the slots actually
    referenced); let Polydat provenance re-evaluate; read each
    declared output and write it through to the op's main
    GkState. Map-shape adds the composite-wire assembly step
    (collect each entry's typed value, project to JSON AST,
    materialise as a single typed-map wire).
- `nmbrs-runtime/src/wrapper_registrations.rs::trigger_result`
  flips from "result map non-empty" to "result spec
  present and non-empty."
- Tests:
  - String shape: `regex_match(body, …)` true/false against
    a TextBody.
  - String shape: `has_results := count > 0 and len(body) > 0`.
  - Map shape: composite map wire materialises with correct
    typed JSON-AST representation.
  - List shape: fragments concatenate; mixed string/map
    fragments produce both flat wires and the composite.
  - Reference to a captured field plus `body`.
  - Reference to an outer-scope `shared` wire bound via
    `extern`.
  - Compile error: unbound identifier names wire + line.
  - Reassignment of `body` is a hard error.
  - Empty-body adapter (stdout) emits Warn under normal mode
    when an expression references `body`.
  - Map-shape key-collision across list fragments errors at
    workload-load.

### Push 3 — Workload migration

- `nmbrs/workloads/cql/full_cql_vector.yaml` rewrites per
  §"Worked example". `cql_dialect` param deleted; `if:` gates
  removed; four ops → two ops.
- Smoke run against a Cassandra 5 cluster confirms the SAI
  path; if a vendor-shaped fixture exists, smoke that too. (The
  user's environment dictates which clusters are available;
  the test matrix is operator-driven, not CI-gated.)

Pushes 1 and 2 commit independently. Push 3 depends on both —
the workload uses both `pick` and the result-bindings form,
plus `exactly_one_value` and `log_info` from the Push 1 set.

---

## Decisions made

- **`result:` is vari-structured (string / list / map).**
  Three YAML shapes with distinct intent: string is GK
  source (`bindings`-style), map is named-key short-forms
  with a composite-map output, list is a sequence of
  either. The shape on the page signals intent; the runtime
  parses each accordingly. SRD-40b §5.1's mapping form
  survives as the map shape with two refinements: polydat-call
  magic-detection drops, and the composite-map output is
  added.
- **Closure bindings follow the standard Polydat linkage-detection
  rule.** Result-bindings extern slots (`body`, `count`,
  `ok`, captures) are instantiated only where Polydat module
  matter detects linkages — same economy `bindings:` already
  uses. No unconditional pre-declaration; no per-cycle cost
  for slots the source doesn't reference.
- **No implicit modal body projection.** Adapters expose
  body as a structural value; workloads that want the unary
  text content write `exactly_one_value(body)` (Surface 4).
  Implicit "unwrap if shape matches, JSON-stringify otherwise"
  was rejected — it makes identical workload source produce
  different behaviour against different bodies.
- **`exactly_one_value` and `log_*` are stdlib node
  functions.** Land in Push 1 alongside `pick`. They compose
  inside any Polydat expression, not just result bindings — pure
  primitives, not result-wire-specific magic.
- **`pick` takes split-halves args, not interleaved pairs.**
  See §"Why not pair-wise `(b, v)` interleaving?" Cleaner at
  call site for long lists; better diagnostic on missing
  pairs.
- **Multiple-true and zero-true are eval-time errors, not
  silent defaults.** Workload authors need a clear failure
  signal when their probe assumptions break. A silent
  "default to first value" or "fall through" would mask
  cluster-misidentification bugs at runtime.
- **No "first write wins" guard on `shared` writes.** The
  SRD-16 last-write-wins semantic stays. A re-running probe
  overwrites; that's the right behaviour for resume scenarios.

---

## Open questions

(The questions below have been folded into the body of this
SRD per the inline review responses; they're kept here as a
record of the design decisions.)

1. **Zero-pair `pick`** — rejected. `min_wires: 2` stays.
   See §"Variadic registration."
2. **Magic-extern injection rule** — follow the standard
   "closure bindings in inner Polydat scopes are made only where
   Polydat module matter detects linkages" rule. The runtime
   doesn't pre-inject `body` / `count` / `ok` unconditionally;
   it inspects the result-bindings source's free identifiers
   and binds only the ones referenced. See §"Compilation
   lifecycle."
3. **Body-shape projection** — no implicit modal "is this
   unary? then unwrap" behaviour. Workload authors write
   `exactly_one_value(body)` (or similar — see §"Surface 4")
   to assertively unwrap. See §"Surface 4 — `exactly_one_value`
   and structured-body assertions."
4. **Logging from result bindings** — out of scope as a
   status-line concern, but the Surface-4 set adds
   `log_debug` / `log_info` / `log_warn` Polydat node functions for
   the side-effect-logging case. See §"Surface 5 — Logging
   node functions."
5. **Cycling re-eval** — every cycle's source re-evaluates
   against the latest body. SRD-16 last-write-wins semantics
   carry through. Workloads that want a one-shot probe set
   `cycles: 1`.

---

## Push 2 follow-up: kernel-driven path

Discovered during the partial Push 2 implementation. Anyone
wiring the kernel-driven path next needs to address these:

### Design constraint 1 — the extern-vs-result-wire collision

`bind_outer_scope` (`polydat/src/kernel/polydatkernel.rs:369`)
attaches an outer-scope `shared X := init` cell to an inner
kernel's input slot ONLY when the inner kernel already
declares `extern X: T`. There's no auto-creation of input
slots; the inner kernel must opt in.

That means for a result-wire `X := <expr>` to write back to
an outer-scope `shared X`, the op's main kernel needs an
`extern X: T` declaration for X to have an input slot — and
the result-wire's eval needs to write to that slot via
`state.set_input()`.

**The trap:** if the op's `bindings:` declares `extern X: T`
AND the result-wire source assigns `X := <expr>`, naïve
kernel synthesis tries to create both an extern passthrough
node and a binding-output node with the same `__port_X`
name. The assembler rejects this with "duplicate node name."

**The right fix:** kernel synthesis must detect when a
result-wire LHS matches an existing extern declaration and
treat the assignment as "compute this expression, write the
value to the extern's input slot at eval time" — not as a
new output-node declaration. This is a small but pointed
change to the DSL compiler's `compile_binding` path: when
the LHS name matches an existing `extern` decl in the same
scope, emit a `__write_<name>` node (or similar) that holds
the computed value and updates the extern's slot via
`state.set_input` per cycle.

### Design constraint 2 — auxiliary-kernel vs. main-kernel

Two architectural shapes considered:

**A. Compile result-bindings into the op-template kernel.**
Append `extern body: Str` / `extern count: U64` / `extern
ok: Bool` plus the user's source to the op's bindings before
kernel synthesis. The kernel re-evaluates per cycle when
the runtime sets these inputs. This is the SRD's preferred
shape (§"Compilation lifecycle"), but requires the constraint-1
fix.

**B. Build a separate auxiliary kernel.** Compile the result-
bindings as a standalone PolydatKernel attached to the
ResultDispenser. Per cycle, set body/count/ok inputs, lookup
each declared output, write to OpResult.captures, let the
existing capture plumbing route values to the op's main
GkState (where SharedCell propagation happens via
`bind_outer_scope`). Avoids constraint 1 (no kernel-synthesis
change), but requires per-fiber aux-kernel state cloning
(GkState is per-fiber today) and breaks the "result-bindings
read from outer scope freely" property (the aux kernel
would need its own `bind_outer_scope` against the workload-
root or op-template kernel).

Shape A is cleaner architecturally; B is more incremental.
Either works for the SRD-66 use case; pick during the
implementation pass.

### Design constraint 3 — `body` projection per adapter

The default `body.to_text()` returns `serde_json::to_string`
of the body's JSON form. For CQL `DESCRIBE KEYSPACE`, the
result is a single row with a single text column carrying
the schema text — JSON-serialised, that becomes
`[{"create_statement":"<text>"}]` with the actual schema
escaped inside. Regex matching against THAT shape is messy.

The right fix: CQL's body wrapper overrides `to_text()` to
return the text-column value when the result is unary, and
the `serde_json::to_string` form for multi-row results.
This is the implicit-modal projection the user explicitly
rejected — instead, workload authors compose
`exactly_one_value(body)` to assertively unwrap a unary
structure (Surface 4). For that to work, `body` needs to
be a structural value (not Str), so `exactly_one_value` can
inspect the row × column shape.

Settling on the body Value variant (Str vs Json vs a new
`Body` Value type) is the gate for both `exactly_one_value`
and the kernel-driven path's body extern.

---

## See also

- SRD-12 — Polydat stdlib (where `pick`'s function metadata lives).
- SRD-13d — Op-template scope (the `extern`/`shared` binding
  layer this SRD's contract rides on).
- SRD-16 — Mutability rules (the load-bearing `shared`
  semantics).
- SRD-34 — Capture points (the existing within-stanza data
  flow that result wires extend).
- SRD-40b — Synthetic metrics from GK; §5 specifies the
  `result:` block this SRD extends.
- SRD-50 — CQL adapter (the first consumer; the
  `describe keyspace` op runs through its statement path).
