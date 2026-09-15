# SRD-46 — Reports: plots, tables, and the unified `report:` block

**Status:** normative
**Owner:** runtime / runner / nmbrs-workload
**Implementation:** `nmbrs-workload/src/report.rs` (parser),
  `nmbrs/src/report.rs` (markdown assembly),
  `nmbrs/src/plot_metrics.rs` (plot rendering),
  `nmbrs/src/summary.rs` (table rendering)
**Cross-refs:** SRD-04 (umbrella options), SRD-15 (strict mode),
  SRD-20 (workload model), SRD-40 (metrics), SRD-45 (sessions)

---

## What a report is

A *report* is the set of named, renderable outputs a workload
declares. There are four item kinds:

- **plot** — image rendered from a metrics-db query.
- **table** — markdown table aggregated from metrics-db rows.
- **text** — markdown prose embedded in the assembled report.
- **file** — scope directive that switches the output file
  for subsequent items in the same group.

Plot and table items are *figures* — they carry data and earn
a figure number. Text is prose; file is meta. Neither
participates in figure numbering.

Reports replace the legacy split between `plot:` and
`summary:` blocks. The terms *summary* and *plot-spec*
no longer name top-level concepts in the workload schema.

---

## The `report:` block

```yaml
report:
  defaults:
    palette: wong
    width: 1024
    height: 640

  intro_section: |
    file overview.md as 'Overview'
      text This is the operational summary for the demo
       workload. It contains a short narrative followed by
       a single chart.
      plot quick_view
        recall@10.mean over limit
        label "Quick recall@10 sweep"

  recall_block: |
    plot recall_at_k10
      over limit by profile
      where dataset=example
      label "Recall@10 vs k limit"
      series profile=hnsw {"line": "dashed", "marker": "triangle"}
    table recall_summary
      metric=recall@.* group_by=profile
      label "Recall summary"

  latency_block: "plot latency_p99 over rate label 'p99 latency'"
```

`report:` is a YAML mapping. Each child is either:

- the reserved key `defaults` — a mapping of style/metadata
  directives that cascade into items at this level, or
- a **group** — a user-named container whose value is a
  directive string (single-line or multi-line block scalar).

A group's value is one or more **items**. Each item begins with
a kind keyword (`plot`, `table`, `text`, or `file`) and
continues until the next kind keyword or end of string. For
`plot` and `table` the next token is the canonical name; for
`file` it's the output filename; for `text` the body starts
immediately after the keyword (auto-named `text_NNN`).

The group key is a YAML-level organizational label that becomes
a markdown section heading.

---

## File scoping (text + file kinds)

A `file <filename> [as '<label>']` directive opens a new
output-file scope. Every subsequent item in the same group
inherits the filename as its `target_file` until either:

- another `file` directive starts a new scope, or
- the end of the group's body.

Items declared *before* any `file` directive in a group land
in the default `summary.md`. Indentation is purely cosmetic —
the parser strips leading whitespace and tracks scopes by
order, not by indent level.

Each item carries a resolved `target_file: Option<String>`. The
markdown assembler routes that item's section into the named
file; PNGs and other on-disk artifacts still go to the session
directory regardless. A file reference (`![](plot_X.png)`)
inside the markdown is what differs between `summary.md` and a
named file.

`text` items are anonymous — auto-named `text_NNN` globally
across the whole document so persistence keys
(`report.<name>`) stay unique. Their body is verbatim
markdown; the parser doesn't try to extract style directives
from prose. A `label "..."` line at the top of the body sets
the section heading.

`file` items are scope directives, not renderable artifacts.
They appear in the listing and in persistence (so the round-
trip is faithful) but render nothing themselves; the items
they scope produce all the output.

### Shared file targets — the "net" rule

Multiple `report:` blocks across a workload — at the same
scope or across scopes (workload root + scenario + phase + op
template) — may reference the same target filename.
Renderers always operate on the **net** union of all items
that target a given file: each item writes its own anchored
section to the file, in declaration order across the resolved
item list. There is no "first writer wins" or clobber
behavior; the markdown assembler upserts each section by its
`{#anchor}` token, so re-running an item replaces its prior
content in place while leaving every other contributor's
section untouched.

Concretely:

- Two scenario reports both declaring
  `file recall_summary.md` will render into the same file,
  with each scenario's text/plot/table items appended in the
  order their scopes activate.
- The auto-injected Run Details block lands in every distinct
  target file, always as the first `##` section.
- An item's canonical name is its anchor key, so item names
  must remain unique within the workload (the existing
  uniqueness rule). Two items with the same name targeting
  the same file would collide on anchor and the second write
  would overwrite the first.

---

## Output routing — the `to` directive

An item declares where its rendered form is delivered:

```yaml
report:
  defaults:
    to: sessiondir          # cascades to every item below

  phases_block: |
    table phases
      to stdout, sessiondir
      query: rows: last(result_success)

  noisy_block: |
    table debug_detail
      to none               # keep the declaration, suppress the output
```

Destinations:

| destination  | effect |
|--------------|--------------------------------------------------|
| `sessiondir` | the standalone artifact plus the markdown upsert into `summary.md` (or the item's `file` target) |
| `stdout`     | the console form on stdout |
| `stderr`     | the console form on stderr |
| `none`       | render nothing — suppresses the item without deleting it |

`session` and `session_dir` are accepted spellings of
`sessiondir`; `off` is accepted for `none`. A list is comma- or
whitespace-separated. `none` anywhere in a list wins outright —
it is a suppression, not a peer.

`to` rides the same cascade as the style directives (report
`defaults` → group `defaults` → item), so one line can route a
whole report. Unlike a style field, an inner declaration
**replaces** the outer set rather than unioning with it: routing
you cannot take back would make an outer default a trap.

### The default is files, never stdout

**An item that declares no `to` goes to `sessiondir` only.**

Automatic end-of-run rendering never writes a report to a
terminal unless the workload asked for it. A run's stdout is its
*op output* — the thing a workload exists to emit — and a report
carries wall-clock values (`span()`, latency percentiles) that
differ between any two runs. Appending one to stdout by default
makes a workload's output non-reproducible, which breaks every
consumer that compares two runs of the same workload (the SRD-105
differential battery is one such consumer).

Two consequences worth stating plainly:

- Presence of a `_summary.md` artifact in the session directory
  means the `sessiondir` destination was selected, and **nothing
  more**. It never by itself implies a stdout write.
- Output routed to `stdout` while a TUI owns the terminal is
  deferred to `.report_stdout.md` in the session directory and
  flushed by the post-run printer once the terminal is back in
  cooked mode. Routing is decided in one place; the flush is
  mechanical.

### Explicit invocation

`to` governs *automatic* rendering. Typing `nmbrs report` is a
request to see the report, so an explicit invocation renders to
the session directory **and** stdout regardless of what the
workload declared — otherwise a workload declaring
`to: sessiondir` would make an explicit report command appear to
do nothing.

`--to <list>` overrides on either path, and takes the same
vocabulary:

```
nmbrs report all session=<dir> --to sessiondir   # write, don't print
nmbrs summary ... --to stderr                    # keep stdout clean
```

---

## Hierarchical placement

`report:` blocks may appear at four scopes, each obeying the
same grammar:

| Scope | Live when |
| --- | --- |
| Workload root | always |
| Scenario | the scenario is the active scenario |
| Phase | the phase runs |
| Op template | its dispenser tallied non-zero ops |

Op-template activation rides on the per-dispenser metrics that
already track counts per (phase, op-template). No new state.

A report declared in a workload is **workload-scoped**: its data
query is narrowed to the declaring execution's `exec_id`, so it
reads only that execution's rows even when the session holds
several executions (a refine sequence, or SRD-88 concurrent
executions). Session-level rollups (`session_summary`) stay
session-scoped. See SRD-88 §5b for the mechanism.

---

## Style and metadata directives

| Directive | Purpose | Plots | Tables |
| --- | --- | --- | --- |
| `as <stem>` | Override default filename stem | yes | yes |
| `label "<text>"` | Display title / caption | yes | yes |
| `palette <name\|N>` | Color sequence | line colors | cell-highlight colors |
| `line=<style>` | Line dash: `solid`, `dashed`, `dotted`, `dashdot`, `none` | yes | ignored on cascade |
| `width=<n>` | Stroke width (px) | yes | ignored on cascade |
| `marker=<shape>` | Point shape: `auto`, `none`, `circle`, `square`, `triangle`, `diamond`, `plus`, `cross`. `auto` cycles a distinct shape per series (circle→square→triangle→diamond, wrapping) — literature-style, so lines are distinguishable in grayscale; the legend key carries each series' shape. A per-series `marker=<shape>` override still wins. | yes | ignored on cascade |
| `size=<n>` | Marker radius (px) | yes | ignored on cascade |
| `color=#RRGGBB` | Override per-figure / per-series color | yes | default text color |
| `over <x>` | X-axis label key | yes | yes |
| `by <key>[,...]` / `by *` | Series discriminator(s) | yes | yes |
| `where <k>=<v>[,...]` | Row filter | yes | yes |
| `agg=<fn>` | Aggregation: `mean`, `min`, `max`, `p50`, `p99` | yes | yes |
| `xlabel="..."` / `ylabel="..."` | Axis labels | yes | n/a |
| `xscale=<linear\|log>` / `yscale=<linear\|log>` | Axis scale | yes | n/a |

Directives that don't apply to a kind on cascade are silently
ignored. A directive that doesn't apply to a kind on a
**per-item** declaration warns (the user clearly meant
something).

---

## Per-series style sub-blocks

Per-series overrides accept two equivalent forms:

```
# JSON form — required when curly braces appear
series profile=hnsw {"line": "dashed", "marker": "triangle"}

# Directive form — no braces, key=value list
series profile=hnsw line=dashed marker=triangle
```

The brace form is required to be **strict JSON**. This hooks
into the existing rule: Polydat parameter syntax `{...}` is by
definition not valid JSON, so anything `{...}` that *does*
parse as JSON is unambiguously not GK. The report parser
tries JSON first; failure is a parse error pointing at the
JSON syntax error, never a fallback to GK.

This rule generalizes: any style-like sub-block delimited by
`{...}` anywhere in the report grammar must be strict JSON.
Single uniform rule, no per-parser carve-outs.

---

## Cascade

Outermost → innermost, last wins:

1. `report.defaults` at workload root
2. `report.defaults` at scenario / phase / op-template scope
3. Group-level `defaults <directives>` line
4. Item body (`plot <name> <directives>`)
5. Per-series sub-block

`as` and `label` apply only at the per-item level (not
inheritable defaults).

Group-level defaults use a directive line, not a mapping:

```yaml
report:
  recall_block: |
    defaults palette=tol_muted width=800
    plot recall_at_k10 ...
    table recall_summary ...
```

The `defaults` line is a directive that applies to every item
following it within the same group.

---

## Palettes

Eight built-in colorblind-safe palettes, sorted alphabetically
for stable indexing. White-background-friendly.

| Index | Name | Source |
| --- | --- | --- |
| 0 | `cividis_5` | NASA / perceptually uniform |
| 1 | `ibm` | IBM Design 5-color |
| 2 | `tol_bright` | Paul Tol bright 7-color |
| 3 | `tol_high_contrast` | Paul Tol 3-color |
| 4 | `tol_light` | Paul Tol light 9-color |
| 5 | `tol_muted` | Paul Tol muted 10-color |
| 6 | `viridis_5` | Discrete sample, perceptually uniform |
| 7 | `wong` | Okabe-Ito 8-color (default) |

Numeric usage: `palette=3` → `tol_high_contrast`. Indices are
0-based and stable while the set is fixed; adding a palette
shifts indices downstream of the insertion point. Tab-complete
recommends the named form.

---

## Figure enumeration

Items are auto-numbered at workload-resolve time. **One global
sequence** per source — `figure 3` always means the third
item, plot or table doesn't matter. 1-based. Order is
declaration order across the active `report:` set: scope order
(workload root → scenario → phase → op) then YAML declaration
order within each scope.

Numbering covers every defined item, including inactive ones,
so the number is stable regardless of which scenario/phase ran.

Markdown rendering uses the global number with kind annotation
and an explicit anchor:

```markdown
## 3. Latency P99 (plot) {#latency_p99}

![](plot_latency_p99.png)

## 4. Latency Summary (table) {#latency_summary}

| profile | p99 |
| --- | --- |
| ... |
```

The anchor form `{#name}` is recognized by Pandoc, MkDocs,
Hugo, mdBook; falls back gracefully on plain GFM (the literal
text is harmless and GFM auto-anchors from heading text).

Group keys render as section headings at the next outer level:

```markdown
## Recall Block {#recall_block}

### 1. Recall At K10 (plot) {#recall_at_k10}
...
```

---

## Cross-session subsetting

When the resolved source covers multiple runs (e.g., a
session db that holds items from several `nmbrs run`
invocations), numbering goes hierarchical: `<run>.<item>`.

```
Run 1 (session=baseline_2026-04-01):
  1.1 — recall_at_k10        plot   "Recall@10"
  1.2 — latency_p99          plot   "p99 latency"
Run 2 (session=baseline_2026-04-02):
  2.1 — recall_at_k10        plot   "Recall@10"
```

Single-run sources omit the run prefix. Bare `figure 3` in a
multi-run source is an error: "ambiguous: specify `run.item`
(e.g., `1.3`, `2.3`)".

---

## Name collisions

Two items with the same canonical name in different scopes
(workload root, scenario, phase, op) are auto-disambiguated by
**scope-prefixing** in all output: filename, markdown anchor,
db-stored key. The marker is double underscore (visible,
unlikely to collide with user-chosen names).

```
root__recall_at_k10
phase_bench__recall_at_k10
```

Parse-time warning:

```
report item 'recall_at_k10' is defined at multiple scopes
(workload root, phase 'bench'); output names will be
scope-prefixed (root__recall_at_k10,
phase_bench__recall_at_k10). Rename one to remove the prefix.
```

`--name recall_at_k10` (bare) errors with "ambiguous; did you
mean: ...". The qualified form works.

---

## Auto-rendering

At end-of-run:

- If the workload completed without being aborted by the error
  handler or another fault, render every `report:` item
  attached to *active* components.
- If the workload aborted, render nothing. The workload didn't
  trust its own outputs, so neither does the runtime.
- Each rendered item is delivered to the destinations its `to`
  directive names, defaulting to `sessiondir` — see
  [Output routing](#output-routing--the-to-directive). Nothing
  reaches stdout here that the workload did not ask for.

---

## Persistence

Every `report:` item the workload defines is persisted into
the session db at run time, keyed `report.<canonical_name>`
(flat, kind embedded in the spec body). Persistence covers
all defined items, active or not — so post-run
`nmbrs report ...` against the db sees the full set even when
no `workload=` is given.

---

## Strict mode

The following promote from warn → error under existing
`--strict` / `strict=true` (SRD-15):

- Bare directive keyword used as a child key of `report:`
  instead of inside `defaults`
- Name collision across scopes
- Empty group (`recall_block: ""`)
- Per-item directive that doesn't apply to the item's kind

Always-error, strict-independent:

- Item without leading `plot` / `table` keyword
- JSON style sub-block that fails to parse as JSON
- Item kind keyword followed by a name that collides with a
  directive keyword

---

## CLI

### Primary surface

| Form | Behavior |
| --- | --- |
| `nmbrs report` | List every defined item. No rendering. |
| `nmbrs report all` | Render every item. |
| `nmbrs report <glob>` | Render every item whose name matches the glob. |
| `nmbrs report figure <N>` | Render by global index. |
| `nmbrs report plot <glob>` | Kind-filtered name lookup. |
| `nmbrs report table <glob>` | Kind-filtered name lookup. |
| `nmbrs Polydat visualize <expr\|file.polydat>` | Polydat expression visualizer. Sibling of `polydat functions` / `polydat dag`. Unrelated to `report`. |

All forms accept `workload=<file>` positionally or fall back
to `logs/latest/metrics.db`'s persisted items when no source
is given.

Listing format:

```
1 — recall_at_k10        plot   "Recall@10 vs k limit"
2 — recall_summary       table  "Recall summary"
3 — latency_p99          plot   "p99 latency vs rate"
4 — latency_summary      table  "p99 latency aggregates"
```

JSON style sub-blocks in the listing render normalized to
directive form. The on-disk source stays as the user wrote it.

### Unadvertised aliases

Not in `--help`, not in top-level command tab-completion:

- `nmbrs plot <glob>` ≡ `nmbrs report plot <glob>`
- `nmbrs table <glob>` ≡ `nmbrs report table <glob>`

### Removed

- `nmbrs summary` — gone. No alias.
- `nmbrs plot polydat` — gone. Renamed to `nmbrs Polydat visualize`.

### Tab completion

Pervasive across all forms:

- `nmbrs report <TAB>` → `all`, `plot`, `table`, `figure`, plus
  the union of all named items.
- `nmbrs report plot <TAB>` → plot-kind names only.
- `nmbrs report table <TAB>` → table-kind names only.
- `nmbrs report figure <TAB>` → numeric range hint
  (e.g., `1..N`).
- `nmbrs plot <TAB>` / `nmbrs table <TAB>` (aliases) →
  per-kind name filtering.

All providers read from a single
`nmbrs_workload::Report::named_items()` lookup; the file walk
and db fallback are shared.

---

## Output ordering

Always declaration order — scope order (workload root →
scenario → phase → op) then YAML declaration order within
each scope. This holds for `report`, `report all`,
`report <glob>`, the markdown assembly, and the listing form.

---

## Cross-references

Label and body text are opaque. When a label says
"see Figure 3", the runtime does not resolve the reference;
the user wrote `Figure 3` knowing the number. If numbers
shift (workload edit, merged-session re-run), the user
updates. Auto-resolution is a future feature, not a current
guarantee.

---

## Invariants

- Every report item has exactly one canonical name within its
  source. Collisions across scopes are scope-prefixed; within
  a scope, duplicate names are a parse error.
- Numbering is fixed by declaration order at resolve time. No
  randomization, no alphabetical override, no metric-dependent
  ordering.
- No item is rendered without a kind keyword leading its
  declaration.
- No backwards-compatibility shims. The legacy `plot:` and
  `summary:` keys are removed; using them is a parse error
  with a clear "renamed to `report:`" message.
- The strict-mode promotion list is the authoritative set of
  rules that change behavior under `--strict`. Changes to
  that list require an SRD-15 update too.

---

## v2: Canonical metricsql backend

**Status:** Pushes A and B shipped; Push C partial as of
2026-05-08. The legacy report-DSL grammar (`<metric> over
<x> [by <series>] [where <k>=<v>] [agg=<fn>]`) is no longer
the canonical surface — every report block in the canonical
`full_cql_vector.yaml` workload now uses metricsql directly
through the new `y:` / `x:` / `series:` directives. The
metricsql-backed pipeline (`parse → evaluate →
SqliteDataSource`) is wired into both `nmbrs metrics query`
and the report renderers (`plot_metrics.rs` /
`summary.rs`); `nmbrs report` consumes the same pipeline,
giving operators **one query language across the whole
system**. The legacy DSL keywords (`over` / `by` / `where`
/ `agg=`) still parse for back-compat with un-migrated
workloads in `nmbrs/examples/workloads/` and
`nmbrs/workloads/cql/backup.yaml`; their removal is the
remaining Push C work.

### Why

- Two parallel mini-languages drift forever. Today
  `recall@10.mean over limit where k=10 agg=mean` and
  `avg(recall_at_10_mean{k="10"}) by (limit)` say roughly the
  same thing; users have to know which surface they're on.
- Reports get features-for-free that already work in
  `nmbrs metrics query`: `rate`, counter-reset adjustment,
  window extrapolation, vector matching, subqueries,
  `topk`/`bottomk`/`quantile`, `@ start()`/`@ end()`.
- The DSL's SQL builders deduplicate against the
  `SqliteDataSource` adapter — same SELECTs, different
  call paths.

### What changes

**Three layers, each with its own push:**

#### Push A — Canonical metric names (writer-side) — SHIPPED

Every stored family name conforms to PromQL's identifier
grammar `[a-zA-Z_:][a-zA-Z0-9_:]*`. The implementation went
**further than the original spec** by lifting the per-`k`
variants into label dimensions instead of mangling them
into the family name:

| Pre-v2 | Implemented |
|--------|-------------|
| `recall@1.mean`            | `recall` (family) + `{k="1", r="1", name="recall"}` labels |
| `recall@10.p99`            | `recall` (family) + `{k="10", r="10", name="recall"}` labels — with `p99` carried by the summary's stat-suffix resolution |
| `recall@<N>.<stat>`        | `recall{k="<N>", r="<N>"}`; query for the stat is `<stat>(recall{...})` style |
| `control.concurrency`      | `control_concurrency` (family-name flatten as spec'd)         |
| `control_info.concurrency` | `control_info_concurrency`                                    |
| `cycles_total`             | unchanged                                                     |
| `cycles_servicetime`       | unchanged (summary; queried as `cycles_servicetime_p99` etc. via stat-suffix resolution) |
| `errors_total`, `result_*` | unchanged                                                     |

The label-bearing form is more PromQL-natural — operators
write `avg(recall{k="10"}) by (limit, profile)` instead of
selecting from a fan-out of per-`k` family names. The
`recall_at_<N>_<stat>` shape was scoped out and never
shipped; the bare-family + labels shape is canonical.

The change lives in `nmbrs-runtime/src/validation.rs`
(`ValidationMetrics::new` builds the labelled stats family),
the recall observer in
`nmbrs-runtime/src/observer.rs`, and the control reporter
in `nmbrs-metrics/src/controls.rs`. Stored summary-vs-gauge
model is unchanged; only the family-name shape canonicalizes.

**Migration policy:** existing `metrics.db` files written
with `@`/`.` family names are unreadable by the new code.
`metrics.db` is a session-scoped artifact (per SRD-45) —
sessions don't persist across nmbrs upgrades, so this is
tolerable. No legacy-name fallback.

#### Push B — Report renderers consume `Vec<Series>` — SHIPPED

`plot_metrics.rs` and `summary.rs` parse the report-body
string, extract metricsql expressions via `nmbrs_metricsql::
parse`, evaluate them via `evaluate` / `evaluate_range`
against `SqliteDataSource`, and consume the returned
`Vec<Series>`. Both renderers route through this path; the
SQL builders in plot_metrics now sit alongside the
metricsql path during the Push C transition.

**Render-side directives** (which label is X-axis, which is
the series discriminator, palette, axis labels, etc.) live
*outside* the query string — not embedded in it. Plot bodies
in implementation use **per-axis** keys (`y:` / `y2:` /
`y3:` / `y4:`) carrying the metricsql expressions, with
SRD-65's multi-axis surface layered on top. The originally-
spec'd `query:` key was retired in favor of `y:` because
multi-axis charts need one query per axis:

```yaml
plot recall_10_mean
  label "10-recall@R, oracles & PVS"
  legend: br
  x: r
  x-legend: "R"
  x-ticks: avg(recall{k="10",profile=~"label.*"}) by (k,r)
  y1: avg(recall{k="10",phase="ann_query",profile=~"label.*"}) by (k,r,optimize_for)
  y2: avg(recall{k="10",phase="pvs_query",profile=~"default"}) by (k,r,optimize_for)
  y3: avg(overscan{k="10",phase="ann_query",profile=~"label.*"}) by (k,r)
  y-legends: ["oracle-O4[optimize_for]", "PVS_O4[optimize_for]", overscan]
  y-ranges: [[0.0,1.0]]
  y-labels: [recall, overscan]
  with-table: true
  style profile=default:line=dotted
```

`y:` (= `y1:`) is the primary-axis query; `y2:` … `y4:`
are secondary-axis queries (SRD-65). `x:` is either an
axis-label key (a label name carried on the points to
project against) or itself a metricsql expression for an
x-axis-as-data plot. `series:` (where present — usually
implied by `by (...)` in the metricsql) names the series
discriminator.

The metricsql expressions are opaque to the report parser
— they're parsed at render time. Other directives (`label`,
`palette`, `legend`, `xscale`, `yscale`, `with-table`, the
SRD-65 multi-axis vocabulary) sit in the same flat
directive surface.

**Tables work the same way:** the body carries one or more
`query <name>: <metricsql>` lines (one query per output
column). Each evaluates to `Vec<Series>`; the renderer
projects each series's `group_labels` to columns and the
(one) sample value to the cell.

**Range-vector tables:** when a report wants a time-series
table, the metricsql is evaluated as a range query (anchor
over the session window with a step); each series's samples
become rows.

**Acceptance:** every report from `full_cql_vector.yaml`
re-renders correctly; the canonical workload is fully
migrated to the new shape.

#### Push C — DSL deletion + workload YAML rewrite — PARTIAL

**What's shipped:**

- `nmbrs/workloads/cql/full_cql_vector.yaml` — the
  canonical workload — is fully migrated to the new
  metricsql + `y:` / `x:` / `series:` shape. No legacy
  `over` / `by` / `where` / `agg=` directives remain in
  any active block.
- The new directive surface (`y:`, `y2:`, `y3:`, `y4:`,
  `x:`, `x-ticks:`, `x-legend:`, `y-legends:`, `y-ranges:`,
  `y-labels:`, plus the SRD-65 multi-axis stack) is fully
  parsed and rendered.
- `query <name>: <metricsql>` table-column form is wired.

**What's left:**

- Migrate the back-compat workloads — `nmbrs/examples/workloads/
  summary.yaml`, `nmbrs/examples/workloads/metrics/summary/gk_context.yaml`,
  `nmbrs/examples/workloads/metrics/summary/aggregates.yaml`,
  `nmbrs/examples/workloads/metrics/report_text_file_demo.yaml`,
  `nmbrs/workloads/cql/backup.yaml` — off the legacy DSL.
  These workloads are example/test scaffolding rather than
  shipping configurations; they're the last live consumers
  of `over` / `by` / `where`.
- Once those migrate, **delete the legacy DSL parser** at
  `nmbrs/src/plot_metrics.rs:128-449` (the
  `parse_spec`/`parse_over` walkers and the
  `over`/`by`/`where`/`agg=` directive matchers in the
  per-line dispatch loop). The SRD's original "delete at
  `nmbrs-workload/src/report.rs:121-300`" location was
  wrong — the DSL parsing always lived in the renderer
  (plot_metrics), not in the workload chunker. The chunker
  in `nmbrs-workload/src/report.rs::parse_group` (line 467)
  stays; it only splits items by kind keyword.

**Translation reference for the remaining migrations:**

| Legacy DSL form | New metricsql + directives |
|-----------------|----------------------------|
| `recall@10.mean over limit` | `y: avg(recall{k="10"}) by (limit)`<br>`x: limit` |
| `mean recall@10 over limit by profile where k=10` | `y: avg(recall{k="10"}) by (limit, profile)`<br>`x: limit`<br>`series: profile` |
| `recall@1 over limit where k=1, profile=default` | `y: avg(recall{k="1", profile="default"}) by (limit)`<br>`x: limit` |
| `recall@N` (table) | `query recall: avg(recall{k="N"}) by (...)` |

**No backwards compatibility shim** once Push C completes.
Loading a workload with the old `over`/`by`/`where` syntax
will surface a parse error pointing at SRD-46 v2 — same
policy as the legacy `plot:` / `summary:` keys
(per §"Invariants"). Until the DSL parser is removed, the
old syntax keeps parsing for the grace window.

**Acceptance:** `nmbrs report all` against every active
workload produces correct artifacts; the legacy DSL is
removed from `plot_metrics.rs`; the example workloads have
been rewritten or deleted.

### Push order and gating

Pushes were ordered A → B → C with gates between each:

- A unblocked B (B couldn't query the new names until they
  existed).
- B unblocked C (C's YAML rewrite produces metricsql, which
  needs B's renderer to consume it).
- Each push committed independently.

Pushes A and B shipped fully; Push C is partially complete
— the canonical workload is on the new surface but
back-compat workloads remain, and the legacy DSL parser
stays live until the last back-compat workload migrates.

### Out of scope

- **Cross-session subsetting** — already specified in
  §"Cross-session subsetting" above; v2 doesn't change it.
- **Vector visualization** (`polydat visualize`) — unrelated path.
- **Persisted-report rehydration** — the
  `session_metadata` `report.*` rows continue to store the
  body string; only the body's grammar changes.
- **TUI live-report panels** — separate push (gated on the
  SRD-48 `tokio::sync::watch` subscription work).

---

## See also

- SRD-04 — Umbrella options pattern.
- SRD-15 — Strict mode.
- SRD-20 — Workload model.
- SRD-40 — Metrics framework (where `metric_instance.spec` is
  defined).
- SRD-45 — Sessions (where the session db lives).
- SRD-47 — MetricsQL streaming aggregation (the algebra that
  underpins the runtime path; report rendering uses the
  batch evaluator side of the same parser).
- SRD-48 — Continuous-query runtime (the live-report path
  that v2 makes natural to wire up later).
