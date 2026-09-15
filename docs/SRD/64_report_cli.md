# 64: Report CLI  *(DRAFT — for refinement)*

> **Status: proposal.** Companion to SRD-46 (data model).
> Iterating before code lands.

This SRD specifies the **command-line surface** for SRD-46
report items: the `nmbrs report` family, its subcommands,
their dynamic completion behavior, and the
"render-then-promote-to-workload" workflow.

The motivating premise: SRD-46 says what a report *is* —
plots, tables, text blocks, files — and how it lives in the
workload YAML. SRD-46 does not say how a user **discovers**
the valid grammar from the shell, nor how they **iterate**
on a single report item against the latest session before
committing it to the workload. Both are CLI concerns; both
need the same authoritative grammar to drive them.

Cross-refs: SRD-46 (report data model — load-bearing parent),
SRD-45 (sessions), SRD-60 (CLI surface), SRD-44 (checkpoints
for reproducibility), SRD-47/48 (metricsql for plot/table data).

---

## 1. Goals

1. **Discoverability via shell completion.** `nmbrs report
   plot <TAB>` surfaces every valid directive name, and
   each directive's value position offers the canonical
   value list — palette names, agg functions, metric names
   from the active session, etc. There is no SRD-46 directive
   the user can type into a workload YAML that they cannot
   discover via tab-completion at the CLI.

2. **Iterative single-item rendering.** A user can run one
   `nmbrs report plot recall_at_k10 over limit by profile ...`
   and have exactly that one component rendered into the
   active session directory. No surrounding YAML mutation,
   no rerun of the workload — just the renderer pointed at
   the existing metrics db.

3. **Promote-to-workload as an explicit follow-up.** Once
   the user is happy with a rendered component, they can
   re-invoke the same command with `--add` (and optionally
   `--contextual` and/or `--replace`) and the **exact same
   grammar** is recorded into the workload YAML at the
   appropriate scope.

4. **Single source of truth for grammar.** Tab-completion
   suggestions, CLI argument parsing, and YAML report-block
   parsing all consult the same registry of kinds /
   directives / value providers. Adding a new directive is
   one change, visible in three places.

---

## 2. Subcommand surface

`nmbrs report <kind> <name> [directives...]` is the canonical
form. Each `<kind>` is one of the SRD-46 component kinds:

| Subcommand | Kind | Notes |
| --- | --- | --- |
| `nmbrs report plot <name> ...` | `Plot` | renders PNG + section in markdown |
| `nmbrs report table <name> ...` | `Table` | renders markdown table |
| `nmbrs report text <name> ...` | `Text` | prose section; body via `--body` or `--body-file` |
| `nmbrs report file <stem>` | `File` | scope directive; only meaningful with `--add` |
| `nmbrs report details [<name>]` | `Details` | auto-injected run-context block; explicit form lets the user pin its position |

Plus management subcommands:

| Subcommand | Purpose |
| --- | --- |
| `nmbrs report list` | list every named item resolved against `--workload` (or stored in the session db) |
| `nmbrs report all` | render every named item in declaration order |
| `nmbrs report figure <N>` | render the Nth figure-numbered item |
| `nmbrs report show <name>` | print the resolved spec for a named item without rendering |

The existing `nmbrs plot` and `nmbrs table` aliases remain
(SRD-46 mentions them) but are documented as kind-filtered
shorthands for `nmbrs report plot|table`. No new aliases.

### 2.1 Component-name positional

Every render subcommand takes the component **name** as a
required positional. This is the SRD-46 canonical name and is
what gets persisted to the workload on `--add`. Names must be
unique across the workload (SRD-46 §"Shared file targets").

For ad-hoc one-shot renders where the user doesn't care
about a name, `--name auto` (or omitting the positional
entirely) generates a timestamped scratch name like
`scratch_20260505_153012` that lands in the session dir but
is rejected by `--add` unless an explicit name is then
supplied via `--name <stem>`.

---

## 3. Directive grammar — CLI ↔ YAML parity

Every SRD-46 directive that can appear in a report-item body
has a corresponding CLI flag, **with identical semantics and
identical value vocabulary**. The mapping is mechanical:

| YAML directive | CLI flag |
| --- | --- |
| `over <key>` | `--over <key>` |
| `by <key1>,<key2>` / `by *` | `--by <list>` |
| `where <k>=<v>,...` | `--where <list>` |
| `agg=<fn>` | `--agg <fn>` |
| `label "<text>"` | `--label <text>` |
| `palette <name\|N>` | `--palette <ref>` |
| `line=<style>` | `--line <style>` |
| `width=<n>` | `--width <n>` |
| `marker=<shape>` | `--marker <shape>` |
| `size=<n>` | `--size <n>` |
| `color=#RRGGBB` | `--color <hex>` |
| `xlabel="..."` | `--xlabel <text>` |
| `ylabel="..."` | `--ylabel <text>` |
| `xscale=<linear\|log>` | `--xscale <scale>` |
| `yscale=<linear\|log>` | `--yscale <scale>` |
| `as <stem>` | `--as <stem>` |
| `series <key>=<val> {...}` | `--series <key>=<val>:<json>` (repeatable) |
| `defaults <directives>` | not on item subcommand; lives on parent group |

The CLI parser turns the flag-form into the exact same
`ReportItem` AST node that the YAML parser produces. That
AST node is what gets rendered, and what gets serialised
back to YAML on `--add`. There is one shape, one renderer,
one promotion path.

### 3.1 Why flag-form, not directive-string-form

CLIs that accept `nmbrs report plot recall over limit by
profile where dataset=example` look ergonomic at a demo but
fight the shell:

- spaces inside quoted directives don't survive
  word-splitting cleanly,
- shell completion has to know which positional starts the
  directive blob and walk the body parser every keystroke,
- `--add` writing the same string back faithfully requires a
  round-trip serializer with quote-escape rules.

Flag-form ducks all three. The user can type
`--label "p99 latency"` once and never worry about where
that label ends. Tab completion knows `--<TAB>` always
offers directives. `--add` knows each flag maps to one AST
field and serializes back deterministically.

The directive-string form remains valid in YAML (SRD-46 §3
unchanged); the CLI just doesn't accept that flavor.

---

## 4. Dynamic completion

### 4.1 Tap progression

`nmbrs report` is **Tap 2** (secondary commands, post-run
analysis), matching `nmbrs summary` today.

### 4.2 Completion cascade

Completion at each cursor position is decided by three
inputs, in order:

1. **The kind subcommand** in the current command line.
   `nmbrs report plot --<TAB>` offers plot-applicable
   directive flags only; `--xlabel` doesn't appear under
   `nmbrs report table`.
2. **The active session db.** Resolved from
   `--db <path>` / `--session <name>` / `logs/latest`
   (in that order). Metric names, x-axis keys, series keys,
   filter values are sourced from this db's schema and
   sample of recent rows.
3. **The optional `--workload <path>`.** When passed, name
   completion includes already-defined item names (so the
   user can `--replace` an existing one).

### 4.3 Value providers per flag

| Flag | Value provider |
| --- | --- |
| `--over` | distinct label keys observed in metrics db |
| `--by` | same — multi-valued, comma-separated |
| `--where` | `key=` completes to a label key, then `=<TAB>` to its observed values |
| `--agg` | closed set: `mean min max p50 p99 sum count` |
| `--palette` | closed set: `wong cividis_5 ibm tol_bright tol_high_contrast tol_light tol_muted viridis_5` plus numeric indices |
| `--line` | closed set: `solid dashed dotted dashdot none` |
| `--marker` | closed set: `auto none circle square triangle diamond plus cross` (`auto` = per-series shape cycle) |
| `--xscale` / `--yscale` | closed set: `linear log` |
| `--metric` (positional after kind name) | metric names from the active db, filtered by kind |
| `--name` (on `list`/`show`/`figure`/--add) | item names already defined in the workload + items already persisted in the session db |
| `--workload` | filesystem walk for `*.yaml` / `*.yml` |
| `--session` | session names under `logs/` |

Closed sets live in one Rust source-of-truth (likely
`nmbrs_workload::report::vocab`) consulted by both the CLI
parser (for validation) and the completion node (for
suggestions). Adding a new palette adds it once; both
surfaces update.

### 4.4 Discovery walk

The intended flow:

```
nmbrs report <TAB>           → plot table text file details list all show figure ...
nmbrs report plot <TAB>      → (existing names) auto
nmbrs report plot recall_q1 --<TAB>
                            → --over --by --where --agg --label --palette ...
nmbrs report plot recall_q1 --over <TAB>
                            → cycle limit profile dataset ... (label keys in db)
nmbrs report plot recall_q1 --over limit --by <TAB>
                            → profile dataset table optimize_for ...
```

The user reaches the full SRD-46 grammar without leaving
the shell, without reading the SRD, and without
trial-and-error YAML edits.

---

## 5. Output destination

### 5.1 Default (no `--add`)

Each render subcommand writes its output **into the active
session directory** under a deterministic filename:

- Plot: `<session>/plot_<name>.png` plus a markdown stub at
  `<session>/scratch/<name>.md` containing only the figure's
  anchored section. The stub is for review; nothing
  otherwise consumes it.
- Table: `<session>/table_<name>.md`.
- Text: `<session>/scratch/<name>.md`.
- File: render-time no-op; only meaningful at promote time.
- Details: `<session>/scratch/details_<name>.md`.

The `scratch/` subdirectory keeps single-component renders
out of the way of `summary.md` (which is the assembled
report) so users can iterate without churn.

`<session>` resolves via the same precedence as elsewhere:
`--session <name>` → `--db <path>`'s parent → `logs/latest`
symlink → error if none exists.

### 5.2 With `--add`

The render still happens (so the user gets immediate visual
confirmation that the spec is valid). Additionally, the
spec is serialised back into the referenced workload YAML.
See §6.

### 5.3 Standard output

`--stdout` skips file writing for `text` and `table` items
and prints to stdout instead. Plots reject `--stdout`
(binary PNG to a terminal is hostile); `--ascii` is the
plot equivalent and dispatches to the existing terminal
plotter.

---

## 6. Promotion to workload (`--add`)

`--add` records the just-rendered item into a workload YAML
file. The default target is the workload that produced the
active session — discoverable from the session's
`checkpoint.json` (`workload_path` field). Override via
`--workload <path>`.

### 6.1 Anchor selection

The promotion code must pick **which `report:` block** to
attach the new item to. There are three orthogonal ways to
specify an anchor:

| Form | Behavior |
| --- | --- |
| `--add` (no anchor flag) | Workload root — top-level `report:`. Always-safe default. |
| `--add --at <scope>` | Explicit named scope. Forms below. |
| `--add --contextual <mode>` | Derive the anchor from the active session's data. Modes below. |

**`--at <scope>`** names a specific instance:

- `--at root` — explicit form of the bare default.
- `--at scenario:<name>` — named scenario's `report:`.
- `--at phase:<name>` — named phase's `report:`.
- `--at op:<phase>.<op>` — named op-template's `report:`.

If the named scope doesn't exist in the workload, the CLI
errors and lists the candidates that *do* exist.

**`--contextual <mode>`** derives the anchor from the
session's dispatch tree by walking the item's metric
references:

| Mode | Behavior |
| --- | --- |
| `--contextual auto` | Walk → pick the most-specific scope that uniquely covers the data. |
| `--contextual root` | Force root anchor. |
| `--contextual scenario` | Force scenario level — error if the data spans scenarios. |
| `--contextual phase` | Force phase level — error if the data spans phases. |
| `--contextual op` | Force op-template level — error if the data spans op-templates. |

The `auto` walk is deterministic:

1. List every (scenario, phase, op-template) tuple in the
   active session that emitted at least one row matching the
   item's `where`/`by` filter.
2. If the tuples all share scenario+phase+op-template,
   anchor at the op.
3. Else if they share scenario+phase, anchor at the phase.
4. Else if they share scenario, anchor at the scenario.
5. Else (span scenarios) — anchor at root.

The forced modes (`scenario` / `phase` / `op`) run the same
walk but **fail loudly** if the resolved level is coarser
than the requested one. Rationale: when the user names a
level, they're expressing an intent that the data must
support; silently coarsening the anchor would put the
report at a different scope than the user asked for.

`--at` and `--contextual` are mutually exclusive — passing
both is a usage error. The walk is side-effect-free; the
chosen anchor is printed before the YAML write.
`--dry-run` prints anchor + diff without writing.

### 6.2 Group selection

A `report:` block contains user-named groups (SRD-46 §3).
The CLI defaults to a group named `cli_added` if no group
target is specified — keeps round-trip stable, lets the
user move items by hand later. Override with
`--group <name>`.

### 6.3 Name collision

Item names are unique across the workload (SRD-46
§"Shared file targets — the 'net' rule"). The CLI:

- with no flag, refuses to add when a same-named item
  already exists, pointing the user at `--replace` or
  `--name <other>`;
- with `--replace`, overwrites the existing item's spec
  in-place at its existing anchor (regardless of whether
  the chosen anchor matches — the existing site wins, with
  a note printed);
- with `--rename <new>`, renames the new item and proceeds
  without conflict.

`--replace` is the unambiguous flag. No silent overwrite.

### 6.4 Edit mechanics

Workload YAML promotion is an **AST-preserving edit**: the
YAML library reads the file, the report block at the
chosen anchor is mutated, and the file is rewritten. This
must:

- preserve comments and ordering of unrelated keys,
- preserve quote style on existing scalars,
- emit the new item as a single multi-line block scalar
  (`|`) under its group, matching SRD-46's example shape,
- pass `parse_workload` after the edit.

The post-edit roundtrip parse is mandatory; if it fails
the file is restored from the in-memory pre-edit copy and
the CLI errors with the parser's diagnostic.

### 6.5 Backup + locking on every workload mutation

Every workload-mutating CLI invocation (`--add`,
`--replace`, `nmbrs report rename`, future `nmbrs report rm`
etc.) writes a **backup of the pre-edit workload** before
touching the on-disk file. Backups use a stable-name
convention so re-running edits doesn't blow up disk under
auto-generated names:

- Path: `<workload>.bak` adjacent to the workload file.
- A second-most-recent backup at `<workload>.bak.prev`
  (so the user always has the previous-previous state
  available — covers the "I just realised that last
  rename was wrong, let me undo two steps" case).
- Rotation: on edit, `<workload>.bak` → `<workload>.bak.prev`,
  then current → `<workload>.bak`, then write the new
  content. Atomic via rename, so an interrupted CLI never
  leaves an inconsistent backup pair.

Two-deep is the policy floor; deeper history belongs in
git, not on the working tree.

**Cooperative file locking** wraps the read → mutate →
roundtrip-parse → write sequence. The lock is held on the
workload file via `flock(LOCK_EX)` (Unix) /
`LockFileEx` (Windows); on contention the second writer
waits up to 5s then errors with the holding-pid (where
known). Locks are advisory — they protect concurrent
`nmbrs` invocations, not arbitrary editors. An open
editor holding the file in a buffer is the user's
responsibility.

### 6.6 `nmbrs report rename <old> <new>`

In-place rename of an existing item across the workload.
Mechanics mirror `--add --replace`:

1. Locate the existing item by `<old>` name. Error if not
   found.
2. Re-anchor stays at the existing site (rename is a name
   change, not a scope change).
3. Update the item's name token and any
   anchor-cross-references (e.g. shared-target-file
   anchors that name the item — the SRD-46 `{#anchor}`
   tokens in markdown are derived from name and update
   automatically; nothing inside the YAML body needs
   patching today, but the rename pass scans for future
   forward-references).
4. Same backup + lock + roundtrip-parse as §6.4 / §6.5.
5. Reject if `<new>` is already in use unless `--replace`
   is also passed (`--replace` here means "drop the
   existing item under `<new>` and rename `<old>` over
   it"). That's a destructive operation; it's spelled out
   explicitly so it can't happen by accident.

`nmbrs report rename` does **not** render anything — it's a
pure metadata edit. To re-render after rename, run the
appropriate `nmbrs report <kind> <new>` (no `--add`).

---

## 7. The "scratch" lifecycle

Items rendered without `--add` live under
`<session>/scratch/`. They are **session-local**,
disposable, and out of `summary.md`'s assembly path.

`nmbrs report scratch list` lists scratch items in the
active session.

`nmbrs report scratch promote <name> [--add ...]` is sugar
for `nmbrs report show <session-stored-name> --add ...`,
turning a scratch item into a workload definition without
re-typing every directive.

`nmbrs report scratch clean` deletes the scratch directory.

Scratch items don't collide with workload-defined names —
they live in their own namespace per session.

---

## 8. Idempotence + reproducibility

A given `nmbrs report ...` invocation must produce the same
artifact bytes given the same session db. This is already
true of the underlying renderers; the CLI's contribution is:

- not embedding wall-clock timestamps in the artifact body
  unless the user asked (`--include-timestamp`),
- preserving directive order in the YAML emission so a
  promoted spec round-trips through `parse → emit → parse`
  to an equal AST.

### 8.1 Replay against historical sessions

`--session <name>` accepts any session under `logs/`, not
just `latest`. Reports of a six-month-old run remain
renderable as long as the metrics db is intact.

The session's checkpoint.json carries the `workload_path`;
the CLI uses that for `--add` discovery unless overridden.
When the workload no longer exists at that path, `--add`
errors and points the user at `--workload`.

---

## 9. Error semantics

| Situation | Behavior |
| --- | --- |
| Invalid directive | Reject at parse, name the bad token, exit 2 |
| Metric name not in active db | Render with empty data + warn; user often wants this during dev |
| `where`/`by` key absent from db | Same — empty render + warn |
| `--contextual` finds no matching tuples | Error: "no scope in session emitted rows matching this item" |
| `--add` on a workload that fails to round-trip parse | Restore + error |
| Name collision without `--replace` | Error: "name X already defined at scenario:foo; use --replace or --rename" |
| Active session not resolvable | Error: "no active session — pass --session or run a workload first" |

No silent fall-through. Every input is acted on or
explicitly rejected (cf. memory: "Never Ignore Silently").

---

## 10. Implementation pointers

- **Single grammar registry.** A new module
  `nmbrs-workload::report::vocab` exposes:
  - `KINDS: &[Kind]`
  - `directives_for(kind) -> &[Directive]` where
    `Directive { flag, yaml_directive, value_provider, ... }`
  - `value_vocabulary(kind, flag) -> ValueProvider`
  consumed by:
  - `nmbrs/src/completion.rs::report_node()` for completion,
  - `nmbrs/src/report_cmd.rs` for arg parsing,
  - `nmbrs-workload/src/report.rs` for YAML parsing,
  - the `--add` emitter for YAML round-trip.

- **Workload mutation primitive.** `nmbrs-workload::edit`
  (new module) exposes the read → lock → backup-rotate →
  mutate → roundtrip-parse → write transaction as a single
  function. `--add`, `--replace`, and `nmbrs report rename`
  all dispatch into it; tests don't reach for backup files
  individually. Failure at any step restores from the
  in-memory pre-edit copy and rotates the backup pair
  back.

- **AST and emitter symmetry.** `ReportItem::to_yaml_directive_string()`
  is the round-trip serialiser; it must be tested against
  every shape the parser accepts (round-trip property test).

- **Session resolution helper.** Already exists in spirit
  across `replay.rs` / `summary.rs`; consolidate into one
  `nmbrs_runtime::session::resolve_active(args, env) -> SessionHandle`
  that all post-run commands share.

- **Workload edit.** Use a YAML AST library that preserves
  comments. Cannot use `serde_yaml` directly (lossy on
  round-trip); evaluate `yaml-rust2` or `marked-yaml` for
  the AST-preserving path. This is the riskiest piece of
  the implementation — likely a separate push from the
  rendering work.

---

## 11. Resolved design decisions

The following questions were raised during initial review
and resolved before this draft. Captured here so the
rationale survives:

- **Multi-add: not supported.** A single CLI invocation
  promotes at most one item. Chaining via repeated
  `--also` shapes complicates error handling and provides
  no leverage over running the CLI twice.
- **Anchor selection model:** root is the bare-`--add`
  default, `--at <named-scope>` names a specific instance,
  and `--contextual <mode>` derives the anchor from session
  data (`auto` walks to the most-specific unique scope;
  `root` / `scenario` / `phase` / `op` force a level and
  hard-error if the data doesn't support it). See §6.1.
- **`figure N` does not drive `--add`.** Figure numbers
  shift as items move; only stable item names anchor a
  promotion. `nmbrs report figure 3` is render-only.
- **Backup + locking on every workload mutation.** Every
  `--add` / `--replace` / `rename` writes
  `<workload>.bak` (rotating `<workload>.bak.prev` to keep
  a two-deep stable history) under a cooperative file
  lock that serialises concurrent `nmbrs` writers. See §6.5.
- **`nmbrs report rename` is in scope.** Detailed in §6.6
  alongside the other promotion flows; not parked.
---

## 12. Acceptance test sketch

A lightweight end-to-end script that proves the contract:

```text
1. Run a tiny workload with no `report:` block.
2. nmbrs report plot demo --over cycle --metric throughput
   → produces logs/<session>/scratch/demo.md and
     logs/<session>/plot_demo.png. No workload mutation.
3. nmbrs report plot demo --over cycle --metric throughput \
                        --label "Demo" --add
   → re-renders identical artifact + writes
     `report.cli_added.demo: |\n  plot demo\n    over cycle\n    metric=throughput\n    label "Demo"`
     to the workload's root report block. Backup at
     <workload>.bak holds the pre-edit content.
4. nmbrs report plot demo --over cycle --metric throughput \
                        --label "Demo v2" --add
   → errors with "name 'demo' already defined at root; pass
     --replace or --rename".
5. nmbrs report plot demo --over cycle --metric throughput \
                        --label "Demo v2" --add --replace
   → in-place updates the existing block's `label`. Backup
     pair rotates: <workload>.bak → .bak.prev, current
     state → .bak.
6. nmbrs report rename demo demo_v2
   → renames the item across the workload. Same
     backup-rotate transaction as --add.
7. nmbrs report plot demo_v3 --over cycle --metric throughput \
                            --add --contextual scenario
   → errors if no scenario in the active session uniquely
     covers the row stream; otherwise anchors the new item
     at the chosen scenario's `report:`.
8. parse_workload(workload_path) succeeds at every step.
9. Run two `--add` invocations concurrently against the
   same workload: only one wins per round; the other waits
   on the cooperative lock or errors with the holding pid
   after a 5s timeout. Backup pair never appears
   half-written.
```

Plus per-directive completion tests asserting that `<TAB>`
at every prefix offers the closed-set or db-derived value
list documented in §4.3.
