# 72: Workload `extends:` (single-parent composition)

*Status: SHIPPED — implemented in `crates/nmbrs-workload/src/extends.rs`
(audited 2026-06-10).* `load_and_merge` follows the chain with
cycle detection (full-chain diagnostic), applies every merge
rule in the table below on parsed JSON trees, strips `extends:`
from the result, and re-serialises; `parse_workload_from_path`
routes through it so validation and templating run once on the
merged whole (the string-form `parse_workload` rejects
`extends:` with a no-resolution-context error, as specced under
"Implementation hook"). Coverage: in-module unit tests pin the
core rules (per-key child-wins, bindings concat order +
trailing-newline handling, `status_metrics` union with
first-occurrence dedup, `extends` stripping, per-name phase
replace); the file-level 16-case matrix under "Test plan" is
not fully realised as `crates/nmbrs-workload/tests/extends.rs` — the
chain/cycle/missing-target paths are exercised through the
loader but not individually pinned. SRD-85 (2026-06-11) added
catalog-name resolution of `extends:` targets: local-first,
then catalog (extension-stripped exact name, then
namespace-relative for bundled origins), with a fatal
local-vs-catalog ambiguity error; e2e coverage in
`crates/nmbrs/tests/bundled_workloads.rs` pins extends-from-bundled
and bundled-sibling resolution through the real loader.

A workload YAML file MAY declare a single parent workload at the
top level via `extends: <relative-path>`. The parser merges the
parent and child workloads field-by-field before any
normalisation, validation, or scenario-include resolution runs.

The motivating use case is a sibling diagnostic / sweep workload
(see SRD-65 plot multi-axis, SRD-46 reports) that adds scenarios
and report sections on top of an existing production workload
without duplicating its phases, ops, or bindings.

---

## Syntax

```yaml
# child.yaml
extends: ./full_cql_vector.yaml

params:
  rerank_k_strategy: pin_limit       # adds a new param

bindings: |
  rerank_k_value := select_int(...)  # appended after parent's

scenarios:
  query_sweep:                       # new scenario; parent's
    - for: "..."                     # scenarios stay intact
      phases: [...]

phases:
  pvs_query_sweep:                   # new phase; parent's phases
    concurrency: ...                 # stay intact
    ops: { ... }

report:
  sweep_section: |                   # new report section; parent's
    file sweep_report.md ...         # `oracles_section` stays
```

### Field shape

- `extends:` is a single scalar string. List or map values are
  a parse error.
- Path resolution: **relative to the directory of the file that
  contains the `extends:` directive**. Not the cwd, not the
  workload search path. After resolution the path is canonicalised
  (symlinks resolved, `..` collapsed) for cycle detection.
- Missing target file produces `extends: target not found:
  <resolved-absolute-path> (from <including-file>)`. The resolved
  path is included so the user can see what the relative path
  expanded to.

### Where `extends:` lives

Top-level only. `extends:` inside a `phases:` block, a `scenarios:`
entry, or any other nested map is **not** part of this feature
and is a parse error if encountered.

---

## Merge rules

The child's top-level fields override the parent's per the table
below. Fields not present in the child inherit from the parent
unchanged. `extends:` is consumed by the loader and **does not
appear in the merged workload**.

| Field | Type | Rule |
|---|---|---|
| `extends` | string | Stripped from merged result (already consumed). |
| `description` | string | Child wins if present; otherwise parent's. |
| `params` | map<str, val> | Per-key merge; child wins on conflict; new keys added. |
| `bindings` | string \| map | **Concat: parent first, child appended.** Child can reference parent-bound names; child can re-bind to shadow per Polydat's lexical-scoping rule (SRD-13c, SRD-13f). |
| `status_metrics` | list<str> | **Union of parent + child** (concat then dedup, preserving first-occurrence order). |
| `report` | map<str, section> | **Per-section merge.** Child entry replaces parent's entry of the same name; new entries added; entries the child doesn't mention inherit unchanged. |
| `scenarios` | map<str, tree> | **Per-name whole-entry replace.** Child entry replaces parent's by name; new entries added; entries the child doesn't mention inherit unchanged. The child cannot replace a *sub-node* of a parent scenario — it replaces the whole scenario or leaves it alone. |
| `phases` | map<str, def> | **Per-name whole-entry replace.** Same rule as `scenarios`. The child cannot override one op within a parent phase — it replaces the whole phase or leaves it alone. |
| `ops` (top-level, legacy) | map \| list | If both child and parent are map-form: per-name whole-entry replace. If either is list-form: whole-replace (positions are not stable identifiers). |
| `blocks` | map<str, group> | Per-name whole-entry replace (consistent with phases). |
| `tags` | map<str, str> | Per-key merge; child wins on conflict; new keys added. |

### Sequencing semantics

For fields that concat (`bindings`, `status_metrics`):

- `bindings:` — parent's Polydat source is emitted into the
  workload-root kernel **first**, child's source **after**. This
  matches the lexical order a user would get if they manually
  concatenated the two files: parent declarations are in scope
  for child code, and child rebindings shadow parent ones per
  Polydat's existing scope semantics. No special "super" reference.

- `status_metrics:` — union semantics, first-occurrence order.
  `[a, b, c]` (parent) + `[b, d]` (child) = `[a, b, c, d]`. The
  child cannot remove a parent's glob; it can only add new ones.
  If a workload needs to opt out of a parent's status pattern,
  the parent should not have hard-coded it (see
  [[feedback_no_presumed_features]]).

---

## Chain semantics

Parent files MAY themselves declare `extends:`. The loader
resolves the chain bottom-up:

1. Recursively load the deepest ancestor first.
2. Each descendant applies its merge rules on the accumulated
   parent.
3. Validation, normalisation, and scenario-include resolution
   run **once**, on the final merged workload.

```
grandparent.yaml          (no extends)
    │
    ▼  merge
parent.yaml               (extends: grandparent.yaml)
    │
    ▼  merge
child.yaml                (extends: parent.yaml)
    │
    ▼
[normalise + validate]
```

Multiple-parent (`extends: [a, b]`) is **not** part of this
feature. A future revision may add it; until then list/map values
on `extends:` are a parse error.

### Cycle detection

The loader tracks canonicalised absolute paths visited during
the recursive load. If a path repeats, the loader emits:

```
extends: cycle detected
  child.yaml
  → parent.yaml
  → grandparent.yaml
  → child.yaml  (cycle)
```

The full chain is shown so the user can pick the link to cut.

### Error reporting

Errors in a parent file are wrapped with the chain that led to
the load:

```
while loading <child.yaml>'s parent <parent.yaml>:
  YAML parse error at line 17: unexpected character ':'
```

The original parser error is preserved verbatim. Only the
"while loading … " preamble is added.

### Validation timing

Schema validation, scenario-include resolution
(`resolve_scenario_includes`), and `expand_templates`
template-folding run **once** on the final merged workload.

Consequence: parent files MAY be intentionally partial. A parent
with no `scenarios:` and no top-level `ops:` is valid as long as
the chain's tail produces a complete workload. This makes
"abstract base" parents possible without forcing every parent to
be runnable on its own.

---

## Implementation hook

Entry point: `crates/nmbrs-workload/src/parse.rs::parse_workload`.

The merge step inserts between **Stage 1 (template expansion)**
and **Stage 2 (YAML→JVal parse)**, conceptually — but in practice
the merge has to happen on the **parsed `JVal` tree**, not on
raw text, because the merge rules are structural (per-key,
per-name).

Revised stage order:

1. **Stage 0a — read file.**
2. **Stage 0b — peek for `extends:`.** Cheap top-level YAML parse
   (just enough to extract the `extends:` scalar). If present,
   recursively load + merge the parent first.
3. **Stage 0c — merge.** Apply the merge rules over each
   top-level field of `child_jval` and `merged_parent_jval`.
   Produce a single `JVal` tree with `extends:` stripped.
4. **Stage 1 — template expansion** on the merged tree.
5. **Stage 2..N** — unchanged from current parse_workload flow.

Per-file params at Stage 1: the `params:` map passed to
`parse_workload` already overrides the workload's own `params:`
block. After this SRD, `params:` flows through three layers:
**parent's `params:` → child's `params:` (overrides parent) →
caller-supplied params (overrides child)**. Caller-supplied
params win at the outer-most layer, as today.

### New module / file

A new module `crates/nmbrs-workload/src/extends.rs` MAY house the loader
helper (`load_with_extends(path, visited) -> Result<JVal, _>`) and
the per-field merge functions. Splitting it out keeps
`parse.rs` from growing past the [[feedback_file_size_limit]]
soft limit (it's already at 3116 lines).

Public surface change to `parse.rs`: `parse_workload` gains a
sibling `parse_workload_from_path(path: &Path, params: …)` that
performs the include-resolution before delegating to the
existing in-memory entry point. The in-memory `parse_workload(&str,
…)` entry point keeps working for callers that have already
resolved the source (tests, inline workloads from
[[project_workload_field_contexts]]); a `parse_workload(&str, …)`
input containing `extends:` is a parse error (no resolution
context available).

---

## Test plan

Test file: `crates/nmbrs-workload/tests/extends.rs` (new).

| # | Case | Assertion |
|---|---|---|
| 1 | Single-parent, child adds new scenario | merged has parent + child scenarios |
| 2 | Single-parent, child overrides one phase | child's phase replaces parent's; other phases intact |
| 3 | `bindings:` concat order | child sees parent's name; child re-bind shadows parent |
| 4 | `status_metrics:` union | parent `[a, b]` + child `[b, c]` → `[a, b, c]` |
| 5 | `report:` per-section merge | child adds new section, doesn't touch parent's `oracles_section:` |
| 6 | `params:` three-layer precedence | caller > child > parent |
| 7 | Two-level chain | grandparent → parent → child resolves bottom-up; merged correctly |
| 8 | Cycle detection | a → b → a errors with full chain in message |
| 9 | Missing target file | error includes resolved absolute path |
| 10 | Path relative to including file | nested directory layout resolves correctly |
| 11 | Parent has YAML error | error preamble identifies the child that triggered the load |
| 12 | `extends:` as a list | parse error |
| 13 | `extends:` inside a phase | parse error |
| 14 | Parent partial (no scenarios) but child complete | merged workload validates |
| 15 | Top-level `ops:` map-form merge | per-name override works |
| 16 | Top-level `ops:` list-form merge | whole-replace works |

---

## Out of scope

- **Multi-parent / mixin composition.** `extends: [a, b]` is not
  supported. Linear chain only.
- **Deep-merge inside phases or scenarios.** Whole-entry replace
  only. If you want to vary one op within a parent phase, define
  a new phase that copies the parent's ops and modifies the one
  you want; SRD-71 cursor partitions and SRD-67 Polydat subcontext
  construction give you the GK-level tools to share kernels
  without sharing YAML structure.
- **Super-reference syntax.** No `super:` or `parent:` token.
  Bindings concat gives lexical shadowing for the common case;
  whole-entry replace gives full override; no intermediate form
  is exposed.
- **Per-section overlay tokens** (e.g. `phases_overlay:` that
  deep-merges). The user owns the merge semantics by structuring
  the parent + child accordingly.
- **Schema-validation differences between partial parents and
  full workloads.** Parents are validated as part of the merged
  whole, not in isolation.

---

## Open follow-ups

- Whether `description:` should accumulate (e.g. `child (extends
  parent)`) instead of override. Punted — override is simpler and
  the user can write whatever they want in the child.
- Whether the merged-workload result should record the include
  chain for diagnostic / `nmbrs describe workload` output. Likely
  yes (cheap, useful for "which file did this phase come from").
  Captured here, deferred to implementation.
- Interaction with [[project_workload_field_contexts]] (workload
  field contexts SRD): the document references `metric:`
  expressions and `evaluations.relevancy.expected` field
  contexts. Those live inside phase entries, which are
  whole-replaced, so no interaction. Confirmed during draft.
