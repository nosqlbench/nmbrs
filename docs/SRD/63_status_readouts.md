# 63: Readouts  *(SHIPPED — Explanation overlay pending)*

> **Status: SHIPPED.** The readout engine is built: the `Readout`
> trait, `ReadoutContext` (30+ accessors), the `Lod` / `ContentMode`
> models, 14 built-in readouts in the registry (`phase_outcome`,
> `phase_status`, `scope_header`, `metric`, …), the `ReadoutSink` seam
> with both `StringSink` and `TuiReadoutSink`, both the stateless
> `DefaultBinder` and the stateful `TuiReadoutBinder` (focus / LOD /
> overlay), the baked-step compiler, and the `readout_snapshots` sqlite
> retention table. Code lives in `crates/nmbrs-runtime/src/readouts/`
> (`readout.rs`, `context.rs`, `binder.rs`, `registry.rs`,
> `snapshot.rs`, `parse.rs`) and `crates/nmbrs-tui/src/readout_sink.rs` +
> `readout_panel.rs`. The ONE piece still stubbed is the **Explanation
> overlay** (§3.2): `ContentMode::Explanation` currently renders zero
> bytes for all readouts, pending its push.

A **readout** is a small named unit that renders one piece of
runtime information — throughput, ok-rate, errors, recall,
latency, phase identity, scope nesting, etc. — into text
suited for a status line, summary, or panel. This SRD
specifies the readout abstraction, the context it reads
from, the LOD model it renders against, and the workload-
level surface that picks which readouts run where.

The motivating tension: today every emit site (inline
status, ✓ DONE, TUI active-phase panel, scenario tree row,
post-run summary) bakes its own format. The conventions
have drifted apart and `status_metrics:` made the cost
visible. We replace the per-call-site `format!()` strings
with a small set of named readouts driven by a baked
render-step list, where the workload picks the readout and
the surface owns the layout.

---

## 1. The `Readout` trait

A readout is a pure unit:

```rust
trait Readout {
    /// Stable, lower-snake-case identifier that workloads
    /// reference. Examples: "phase_seq", "phase_name",
    /// "phase_coords", "throughput", "ok_pct", "errors",
    /// "retries", "concurrency", "duration", "eta",
    /// "state_glyph", "progress_pct", "progress_bar",
    /// "metric" (parameterized), "scope_header".
    fn name(&self) -> &'static str;

    /// Render this readout at the given LOD and content
    /// mode, drawing data from the context, into `out`.
    /// Returns the rendered width so the caller can do
    /// alignment without re-measuring. Hot-path: no
    /// allocation beyond what the output buffer requires.
    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        opts: &ReadoutOptions,
        out: &mut ReadoutBuf,
    ) -> usize;
}

enum ContentMode {
    /// The actual data — what the readout normally shows.
    Value,
    /// The explanation overlay (§3.2): same shape and
    /// width as `Value`, but text describes what the
    /// glyphs mean rather than carrying live data.
    Explanation,
}

enum Lod { Compact, Labeled, Expanded }
```

`ReadoutBuf` is the surface-supplied output buffer. The
inline-status thread hands a `&mut String` (ANSI-styled);
the TUI hands a ratatui `Vec<Span>` builder (typed style);
a piped sink hands a writer that strips ANSI. The buffer
type is the surface's choice; the readout writes through a
single trait so it doesn't have to know.

`ContentMode` and `Lod` are orthogonal — every combination
is legal: a readout renders `Value @ Compact`,
`Explanation @ Compact`, `Value @ Expanded`,
`Explanation @ Expanded`, etc. The overlay invariant
(§3.2: same shape across modes) is the readout author's
contract, not a runtime check.

---

## 2. The `ReadoutContext` facade

Every readout draws from the same trait. A single phase-level
implementation surfaces every data source any readout might
need; the surface picks the implementation, the readout
picks the methods it actually uses.

```rust
trait ReadoutContext {
    // Phase identity
    fn phase_name(&self) -> &str;
    fn phase_label(&self) -> Option<&str>;       // workload-supplied
    fn phase_seq(&self) -> Option<(usize, usize)>; // (idx, total)
    fn phase_coords_root_first(&self) -> &[ScopeCoord];

    // Phase state (covers running and completed via
    // a single enum; readouts that only make sense in one
    // state branch on it).
    fn phase_state(&self) -> PhaseState;
    fn phase_started_at(&self) -> Instant;
    fn phase_duration(&self) -> Option<Duration>;  // None = running

    // Counters & rates
    fn cycles_completed(&self) -> u64;
    fn cycles_total(&self) -> u64;
    fn ops_started(&self) -> u64;
    fn ops_finished(&self) -> u64;
    fn ops_ok(&self) -> u64;
    fn errors(&self) -> u64;
    fn retries(&self) -> u64;
    fn ops_per_sec(&self) -> f64;
    fn concurrency(&self) -> usize;
    fn eta_secs(&self) -> Option<f64>;

    // Live aggregates (relevancy, latency families).
    // `metric` readouts go through this surface so the
    // glob-pattern code lives in one place.
    fn live_metrics(&self) -> &dyn LiveMetricSource;

    // Adapter-declared status counters (per-disp).
    fn adapter_counters(&self) -> &[AdapterCounter];

    // Structural visibility — the readout can branch
    // on where it sits in the scope tree without each
    // call site rebuilding the path. See §4 for how this
    // drives `scope_header` / `on_each` / `on_phase`.
    fn nesting_depth(&self) -> usize;
    fn scope_chain(&self) -> &[ScopeFrame];

    // Sparkline / window source for `throughput_sparkline`,
    // latency-bar style readouts, etc.
    fn throughput_window(&self) -> &dyn ThroughputWindow;
    fn latency_window(&self) -> &dyn LatencyWindow;
}
```

The trait is intentionally wide — readouts only call what
they need, and a single phase-level implementation covers
every readout in the registry. Inline status, ✓ DONE, the
TUI panel, post-run summary all use the same trait
implementation, just with different `phase_state()` results.

> **Resolved:** structured-data emit bypasses readouts.
> The inspector socket, JSON sinks, and other programmatic
> consumers read `ReadoutContext` directly. Readouts are
> for human rendering only — they do not double-duty as
> field extractors. (See §11 for the design-decision
> rationale.)

---

## 3. LOD: compact / labeled / expanded

Every readout supports three LOD levels, plus an explanation
overlay at each level.

| LOD       | What it renders | Width budget | Lines |
|-----------|-----------------|--------------|-------|
| `compact` | smallest useful form for a trained operator — single glyph cluster, no labels | tight | 1 |
| `labeled` | fits as much labeled info as the line width allows; wraps to additional lines if needed | provided | 1+ |
| `expanded` | maximum detail; supports auxiliary visuals (error bars, history overlays, context strips) | full | many |

LOD selectors accept name **or** number, interchangeably:
`lod:compact` ≡ `lod:1`, `lod:labeled` ≡ `lod:2`,
`lod:expanded` ≡ `lod:3`. `lod=…` works too. Either
syntax is valid in workload yaml and on the CLI.

### 3.1 Compact-form examples

To pin down what compact means in practice:

| Readout | Compact rendering | Reads as |
|---|---|---|
| `throughput` | `5.23QpS` | 5.23 queries / second |
| `pending_active_complete` | `PAC 34234 234/500 23043` | (pending, active/concurrency, complete) = (34234, 234/500, 23043) |
| `latency_p99` | `p99~232ms` | p99 latency 232 milliseconds |
| `state_glyph` + `phase_name` | `▶run` / `✓run` / `✗run` | running / done / failed phase named "run" |
| `recall_at_10` | `r@10:79.6%` | recall@10 mean is 79.6% |

### 3.2 Explanation overlay

Every readout, at every LOD, has an **explanation overlay**
of identical width / shape that explains what each
glyph / abbreviation means. The user toggles overlays at
runtime; while the overlay is on, the readout's rendered
slot shows the explanation in place of the value. This is
what makes the compact form usable for newcomers — they
can flip overlays on and read what `PAC` means without
leaving the live display.

The overlay is **the same readout, a different content
mode** — not a sibling readout, not a separate trait
method. `Readout::render()` takes a `ContentMode`
parameter (§1); the surface passes `Value` for live data
and `Explanation` for the overlay. The same impl produces
both: same data context, same LOD, same width budget,
same styling rules. The author writes one body that
branches on `mode` for the per-field text:

```rust
fn render(&self, ctx, lod, mode, opts, out) -> usize {
    let throughput = match mode {
        ContentMode::Value       => format_rate(ctx.ops_per_sec()),
        ContentMode::Explanation => "ops/sec".into(),
    };
    // …
}
```

Because the modes share LOD and width budget, the overlay
is guaranteed to occupy the same screen footprint as the
value render. Phase-aware overlays still work — the
context is the same, so an explanation that hides
sections the value render also hides (e.g. `e:` when no
errors) stays accurate.

### 3.3 LOD information monotonicity  *(invariant)*

**For every readout, lower LODs are strict information
subsets of higher LODs.** An individual readout cannot
show data at `compact` that isn't also present at
`labeled`, and `labeled` cannot show data that isn't also
present at `expanded`. Going compact → labeled → expanded
only **adds** information; it never substitutes one
field for another.

A readout that wants different field selections at different
LODs must be split into separate readouts — the engine
does not enforce semantic equivalence across LODs of one
readout, so the only path to "different fields per LOD"
is "different readouts".

Why it's load-bearing:

- **Explanation overlays** (§3.2) stay coherent — the
  same glyph cluster at compact form maps to the same
  meaning at labeled or expanded. Operators learn the
  vocabulary once.
- **Snapshot retention** (§6) works without surprise — a
  `labeled` snapshot is at least as informative as the
  `compact` snapshot of the same instant; zooming the
  display in or out reveals more, not different.
- **Composability** — when a parent readout embeds a
  child at `lod=compact`, the parent author knows the
  child's compact output is a faithful subset of what the
  child would show at `expanded`.

Concretely, a readout's render impls satisfy:

```
fields(compact)  ⊆  fields(labeled)  ⊆  fields(expanded)
```

This is a contract on the readout author, not a runtime
check (the engine has no way to know what "field" means
across arbitrary readouts). Violations are caught in
review, not at compile time.

### 3.4 LOD vs surface density

The TUI's existing LOD control (SRD-62) selects *how
many phases are visible and how much each phase's detail
expands*. Readout LOD is a separate axis — it picks the
rendering of one readout. The surface combines them: a
"compact" workload row in the tree might use
`throughput@compact + latency_p99@compact`; the same
phase's expanded detail block uses `throughput@expanded +
latency_block@expanded`. The surface decides which LOD to
ask each readout for; the readout doesn't see surface-level
density.

---

## 4. Structural visibility & event contexts

Some readouts render *structure* (scope nesting headers, the
`· for_each profile=…` lines today). A readout that emits
structural text needs to know its position in the scope
tree, which the `ReadoutContext::scope_chain()` and
`nesting_depth()` methods supply.

### 4.1 Event-driven readouts

The engine fires readouts at two distinct kinds of
moments. The split matters because **lifecycle events
fire exactly once at a canonical moment**, while
**refresh events fire many times and accumulate**. The
binding rules and snapshot semantics differ between them.

**Lifecycle events** — scenario-tree transitions, every
type delaminated into a `_start` / `_end` pair. Each
member of a pair fires *exactly once* per occurrence of
its subject. No event in this set ever fires twice for
the same `(slot, subject)`.

- `on_session_start` / `on_session_end` — session
  boundaries.
- `on_phase_start`   / `on_phase_end`   — phase
  boundaries. (The ✓ DONE line is at `on_phase_end`.)
- `on_each_start`    / `on_each_end`    — `for_each` /
  `for_combinations` iteration boundaries. (Today's
  `· for_each profile=alpha` header is at
  `on_each_start`.)
- `on_scope_start`   / `on_scope_end`   — non-iteration
  scope group boundaries.

The `_start` / `_end` split is canonical: every lifecycle
type has both halves, and a readout binds to exactly the
half it cares about. We never fire a single event "for
the iteration" and then ask the readout to figure out
whether it's at the start or the end — that recovers the
ambiguity the split is supposed to remove.

**Refresh event** — periodic redraw, decoupled from
state changes. Fires repeatedly while its subject is
in-flight. Drives the live status content.

- `on_update` — fires at the surface's refresh cadence
  (inline status thread: 0.5 s; TUI: per-frame). No
  state has changed; the readout just re-reads its
  context and re-renders.

A readout declares which event kind(s) it answers. The
binding layer deduplicates within `(slot, subject)` for
lifecycle events; refresh events skip dedup entirely.

### 4.1.1 Wildcard binding

A readout can bind to multiple lifecycle slots through a
glob:

```yaml
readouts:
  each_*: scope_bracket          # both on_each_start and on_each_end
  phase_*: phase_outline         # both on_phase_start and on_phase_end
  '*': trace_event               # every event, lifecycle and refresh
```

Wildcard bindings are useful for symmetric markers
(opening + closing brackets of a scope, debug tracers
that run at every event). The render order at each
firing is deterministic: explicit bindings first in
declaration order, then wildcard bindings in declaration
order.

### 4.1.2 Lifecycle state in `ReadoutContext`

The `ReadoutContext` always exposes the event that
triggered the current render:

```rust
trait ReadoutContext {
    /// Which slot fired this render. Required for
    /// readouts bound to multiple events (wildcard
    /// bindings) so they can branch on lifecycle phase.
    fn event(&self) -> Event;
    // … (rest as in §2)
}

enum Event {
    SessionStart, SessionEnd,
    PhaseStart, PhaseEnd,
    EachStart, EachEnd,
    ScopeStart, ScopeEnd,
    Update,
}
```

A readout bound to `each_*` reads `ctx.event()` to know
whether it's the opening or closing render. A readout
bound to a single explicit slot ignores `event()` and
just renders. The `Event` variant is small and copyable;
no boxed dispatch on the hot path.

### 4.1.3 Mapping today's surfaces

Today's `phase_starting` scope-walker → `scope_header`
readout at `on_each_start`. Today's inline status thread
→ `phase_status` readout at `on_update`. Today's ✓ DONE
line → `phase_done` readout at `on_phase_end`. The
deleted phase-starting detail row → `phase_starting`
readout at `on_phase_start` (opt-in).

### 4.2 Structural readout vocabulary

| Readout           | Default slot      | Renders |
|-------------------|-------------------|---------|
| `scope_header`    | `on_each_start`   | `· <iter_var>=<value>` at the current nesting depth |
| `each_close`      | `on_each_end`     | optional closing marker (built-in default: empty) |
| `phase_starting`  | `on_phase_start`  | optional pre-phase line (today's deleted row, opt-in) |
| `phase_status`    | `on_update`       | live status: pct / rate / errors / metrics / ETA |
| `phase_done`      | `on_phase_end`    | full ✓ summary line |
| `scope_open`      | `on_scope_start`  | bracket / heading at scope entry |
| `scope_close`     | `on_scope_end`    | bracket / footing at scope leave |
| `session_banner`  | `on_session_start`| run header line (workload name, params summary) |
| `session_summary` | `on_session_end`  | end-of-run roll-up (totals, error rate, duration) |

This subsumes the current scope-ancestor + phase-row split
into a single event-driven model. Surfaces that don't want
structural readouts (post-run summary, inspector socket)
just don't bind anything to those events.

**No specialized per-metric readouts.** Live metrics
(every `recall_at_K`, `latency_p50/p99/max/mean`,
adapter-declared counters) are reached through a single
parameterized `metric` readout — not through a vocabulary
of named-per-metric readouts. Whatever names the live
metric source surfaces (§2 `LiveMetricSource`) are the
spec; the readout vocabulary stays small and the
parameter is the access path. This is what
"spec-driven" means here: no aliases, no per-metric type
hierarchy, no shorthand readout names that the engine
has to maintain in lockstep with the metric registry.

---

## 5. The `readouts:` workload block

Workload-level configuration uses the keyword `readouts:`.
It is **optional** — a workload that doesn't declare
anything renders against the built-in defaults and gets
exactly today's output.

### 5.0 Three legal forms — selective vs total override

The forms differ in **scope of override**. Every event
slot has a built-in default. Whatever a workload doesn't
explicitly mention falls back to the default; this is what
makes partial overrides clean.

**Form A — single-line scalar: total override of
`on_update` only.** All other slots keep their built-ins.
This is the 95% case: "give me a different live-status
line, leave everything else alone."

```yaml
readouts: phase_status                      # built-in name
# or:
readouts: state_glyph phase_name throughput ok_pct errors duration
```

A bare scalar binds to `on_update`. Either a built-in
readout name or a literal readout body is accepted; the
parser tries the registry first, falls back to body
parsing.

**Form B — mapping: selective override of named slots.**
Each key in the mapping replaces *just that slot's*
default. Slots not present in the mapping keep their
built-in. This is the surgical form: "I want my own
`on_phase_end` line, but the live-status and scope
headers are fine as-is."

```yaml
readouts:
  on_phase_end:   phase_done_quiet         # only this slot overridden
  status_metrics: [recall*, latency*]      # existing shortcut
  # on_update, on_each_*, etc. ⇒ built-in defaults
```

The contrast with Form A: Form A *only* touches
`on_update`; Form B can touch *any* slot, but only the
slots named in the mapping.

A bare value (`phase_done_quiet`) picks a built-in by
name. A mapping value defines a custom readout inline
(see §5.1).

**Form C — mapping with named custom readouts and
bindings.** Top-level keys that are not event slots
register as named readouts the workload can reference from
its own bindings (or that other workloads can pull in
later via includes).

```yaml
readouts:
  on_update:    my_status              # binding to the entry below
  on_phase_end: phase_done

  my_status:                           # custom readout definition
    lod: labeled
    body: |
      state_glyph phase_name @YELLOW phase_coords
      throughput precision=2 "ok:" ok_pct "%"
      metric:recall* metric:latency*
      "ETA " eta
```

The keyword is `readouts:`, not `templates:` — these are
readouts; the term `template` is overloaded with `report:`
(SRD-46) and Polydat templating, so we drop it.

### 5.1 Body grammar — unbraced

Readout bodies do not use `{…}`. Curly braces collide
with both Polydat syntax and JSON parsing rules; a third
brace grammar would be attempted by Polydat first and produce
confusing diagnostics. Instead the body is a sequence of
whitespace-separated **items**, with no surrounding
sigil:

```
state_glyph phase_seq phase_name @YELLOW phase_coords
throughput precision=2 "ok:" ok_pct "%" "e:" errors "r:" retries "c:" concurrency
metric:recall* metric:latency*
"(" duration ")"
```

Item kinds:

1. **Readout call** — a bare identifier matching a
   registered readout name (one of the names in §4.2 or
   the registry; **no specialized per-metric names** —
   live metrics are reached only through the parameterized
   `metric` readout). Followed optionally by
   space-separated `key=value` options, terminated by the
   next non-`key=value` token. Examples:
   `throughput`, `phase_name color=BLUE`,
   `metric pattern="latency_p99" unit=ms precision=0`.
2. **Parameterized form (`name:arg`)** — sugar for the
   readout's primary option. `metric:recall*` ≡
   `metric pattern="recall*"`. This is the canonical way
   to reach a specific live metric; there is no
   `latency_p99`-as-readout-name path.
3. **Quoted literal** — `"ok:"` or `'%'`. Preserved
   verbatim. Used wherever the surrounding text would
   otherwise be ambiguous (e.g. punctuation near
   identifiers).
4. **Color / style directive** — `@NAME`,
   `@#aabbcc`, `[#aabbcc]`. See §5.2.
5. **Bare punctuation** — characters outside the
   identifier set (`(`, `)`, `,`, etc.) at item
   boundaries are kept as literal text. Preferred form
   is to quote them when in doubt; a bare `(` next to a
   readout call is unambiguous.

Whitespace between items renders as a single space; for
explicit run-of-text spacing (or to suppress spacing) use
quoted literals. Unknown readout names and unknown options
are load-time errors (per "Never Ignore Silently").

### 5.2 Color and style

Color and style apply uniformly across literals and
readout output, the same way for every readout. Two
mechanisms:

**Inline directives** — affect the *next* item only,
single-shot. Reset is implicit at the next item or `@_`.

```
@RED errors          # readout output in red
@INFO phase_name     # styled by the INFO style mapping
@#7AC166 "OK"        # literal text in a hex color
[#7AC166] phase_name # bracketed hex form, equivalent
```

**Per-readout option** — `color=NAME` or `style=NAME`
on the readout itself.

```
phase_name color=BLUE
errors style=ERROR
duration color=#888888
```

Recognized name vocabulary:

- **Direct color names** (uppercase): `RED`, `GREEN`,
  `BLUE`, `YELLOW`, `CYAN`, `MAGENTA`, `WHITE`, `BLACK`,
  `DIM`, plus the explicit-bright `BRIGHT_RED` etc.
- **Hex** (`#rrggbb` or `#rgb`).
- **Style names** — semantic, palette-resolved:
  `ERROR`, `WARN`, `INFO`, `OK`, `HEADER`, `SUBHEAD`,
  `EMPHASIS`, `MUTED`. These map to the active palette
  (e.g. `WONG`, the colorblind-safe palette already used
  by SRD-46 reports). Operators / workloads can swap
  palettes; the readout text follows.

Style names are the recommended path because they survive
palette changes and stay coherent across surfaces. Direct
colors and hex are the escape hatch for one-off
emphasis.

### 5.3 LOD inside readout bodies

A readout body can pin a child rendering's LOD via the
same `key=value` option mechanism:

```
throughput lod=compact
latency_block lod:expanded
```

Names and numbers both work — `lod=1` ≡ `lod=compact`,
`lod=2` ≡ `lod=labeled`, `lod=3` ≡ `lod=expanded`. The
colon-shorthand form (`lod:compact`) is also accepted to
match the `metric:pattern` shape. Default LOD comes from
the surface; the body can override per call site.

### 5.3.1 Layout specifier inside readout bodies

In addition to `lod=`, every readout call accepts a
`layout=` option that overrides the binder's default
classification (§7.4) for that one site. Three values:

- `layout=inline` — render compact, share a line with
  any neighbouring inline readouts. Forces
  `LayoutHint::InlineCompact` regardless of the
  effective LOD.
- `layout=block` — render block, claim its own line(s).
  Forces `LayoutHint::Block` regardless of LOD.
- `layout=auto` (default) — let the binder pick:
  compact LOD → inline, labeled / expanded → block.

Examples:

```
# Force a normally-block readout to crowd onto a line
# with its neighbours (e.g. a tight one-row dashboard):
latency_block layout=inline lod=compact

# Force a normally-inline readout to take its own line
# (e.g. emphasis):
phase_name layout=block

# Default — equivalent to omitting the option:
throughput layout=auto
```

`layout=inline` on a readout that produces a multi-line
render is a workload-author error: the binder cannot
honour the inline contract for content that contains
embedded newlines. Detected at workload-load (when the
readout's metadata declares it as multi-line) or at
first-render (when an inline-tagged readout writes a
newline anyway). Either way, a load-time / runtime
warning fires per `feedback_never_ignore_silently`.

Same colon-shorthand form is allowed: `layout:inline`,
`layout:block`, `layout:auto` — matches the
`metric:pattern` and `lod:compact` styles.

### 5.4 Continuity examples for `status_metrics:`

The existing `status_metrics: [recall*, latency*]` shortcut
keeps working by mapping into the new engine. Three concrete
shapes for the same workload, increasing in customisation:

**A. Default — no `readouts:` block at all.** Matches
today's output byte-for-byte.

```yaml
# (nothing — workload doesn't declare readouts)
status_metrics: [recall*]
```

Renders:
```
✓ [1/8] [run] (profile=alpha) 100% 162/s ok:100% e:0 r:0 c:1 recall_at_10:79.62% (0.01s)
```

**B. Pick a different built-in.**

```yaml
readouts:
  on_phase: phase_done_quiet     # built-in: drops pct + retries
status_metrics: [recall*]
```

**C. Custom inline readout.**

```yaml
readouts:
  on_phase:
    lod: labeled
    body: |
      {state_glyph} {phase_name}
      {throughput precision=2}
      {metric:recall*}
      {duration unit=auto}
```

Pattern: workloads that want emphasized metrics use
`status_metrics:`. Workloads that want layout control use
`readouts:`. Both can coexist — `status_metrics:` patterns
are visible to the engine via
`{metric:status_metrics}` inside built-in or custom
readouts.

### 5.4.1 Composition vs. override — disambiguating intent

Multiple readouts bound to the same event slot is a real
configuration that the engine supports (see §5.5's
`on_phase_end: [phase_done, phase_failure_hint]` example).
But the same surface is also the path for *accidental*
collision: a CLI override, a workload import, and a
workload base may all reach for the same slot, and the
author may not realise something they relied on was
silently replaced. We disambiguate intent at the binding
layer with three rules.

**Rule 1 — list is the canonical shape.** A slot's value
is a list of readout invocations. A bare scalar
(`on_phase_end: phase_done`) is sugar for the
single-element list `[phase_done]`. The engine always
walks a list at fire time.

**Rule 2 — _higher layers REPLACE, they don't merge by
default.**_ Layering is closest-wins per §5.0 (CLI
overrides workload-extension overrides workload base
overrides built-in default). When a higher layer binds
the same slot, its list fully replaces the lower
layer's list. No silent merging. This is the only
behaviour that makes "I overrode that on the CLI" work
predictably.

Worked example. Three input layers, four slots involved:

```yaml
# Layer 1 — built-in defaults (registered in
#           nmbrs-runtime::readouts::registry).
on_session_start: session_banner
on_each_start:    scope_header
on_update:        phase_status
on_phase_end:     phase_done
on_session_end:   session_summary

# Layer 2 — workload yaml (the user's workload).
readouts:
  on_phase_end:
    - phase_done
    - phase_failure_hint        # extra readout the
                                #  workload wants on
                                #  every phase end

# Layer 3 — CLI flag (operator override at run time).
$ nmbrs run … --readout=on_phase_end:phase_done_quiet
```

Resolution table (closest-wins per §5.0; lower layers
shown only when a higher layer didn't override):

| slot               | layer 1 — builtin   | layer 2 — workload                  | layer 3 — CLI         | resolved                           |
|--------------------|---------------------|-------------------------------------|-----------------------|------------------------------------|
| `on_session_start` | `session_banner`    | (not declared)                      | (not declared)        | `[session_banner]`                 |
| `on_each_start`    | `scope_header`      | (not declared)                      | (not declared)        | `[scope_header]`                   |
| `on_update`        | `phase_status`      | (not declared)                      | (not declared)        | `[phase_status]`                   |
| `on_phase_end`     | `phase_done`        | `[phase_done, phase_failure_hint]`  | `phase_done_quiet`    | `[phase_done_quiet]` ← REPLACED    |
| `on_session_end`   | `session_summary`   | (not declared)                      | (not declared)        | `[session_summary]`                |

Two consequences worth pinning down:

- **The CLI's `phase_done_quiet` fully replaces the
  workload's two-element list.** `phase_failure_hint`
  does **not** survive into the resolved binding. Without
  this rule, overriding "the phase done line" from the
  CLI would have to know about every readout the
  workload happened to attach and disable each one
  explicitly — an unstable surface.
- **A workload list that replaces only a built-in
  default does NOT warn.** Built-ins exist precisely so
  workloads can override them. Warning fires only when
  a *non-default lower layer* (i.e. the workload, not
  the built-in) is silently dropped.

Because layer 2 *is* a non-default binding, layer 3's
override emits a load-time warning:

```text
warning: readouts: on_phase_end — CLI override
  [phase_done_quiet] replaces workload binding
  [phase_done, phase_failure_hint]. Use a `+` prefix on
  the CLI value (e.g. `--readout=on_phase_end:+phase_done_quiet`)
  if you intended to extend rather than replace.
```

Under SRD-15 strict mode the warning is a hard error and
the operator must re-state intent — either accept the
replacement explicitly via a `--readout-replace` flag, or
opt into Rule 3's `+` prefix to extend instead of
replace.

The same machinery handles intra-layer composition: a
workload that wants the failure hint *plus* the quiet
done renderer writes both out as a list at layer 2, and
no override happens at all:

```yaml
readouts:
  on_phase_end:
    - phase_done_quiet
    - phase_failure_hint
```

Resolves to `[phase_done_quiet, phase_failure_hint]`.
This is the form authors reach for when they know they
want multiple readouts on a slot — explicit, no
inference, no silent additions from any layer.

**Rule 3 — explicit append for the rare merge case.** A
prefix `+` inside the list opts in to merge instead of
replace:

```yaml
readouts:
  on_phase_end:
    - "+phase_failure_hint"   # add to inherited list
```

The `+` prefix means "prepend / append (parser-specified)
to whatever the lower layer bound at this slot, rather
than replacing." Without `+`, the list is total. A list
that contains *only* `+`-prefixed entries is a pure
extension; a list with any plain entry is a replacement
that happens to also include extensions.

**Safety nets:**

- **Silent-override warning.** When a higher layer
  replaces a non-default lower-layer binding (i.e. the
  workload, not just the built-in), the engine emits a
  warning at workload-load: `readouts: on_phase_end —
  CLI override [c] replaces workload binding [a, b]`.
  Per `feedback_never_ignore_silently` — if the user's
  intent was actually composition, the warning tells
  them so before the run starts.
- **Strict mode promotes to error.** Under SRD-15
  strict mode, a silent override is a load-time error;
  the operator must add `+` (composition) or
  re-affirm the override explicitly via a flag like
  `--readout-replace=on_phase_end:c`.
- **Within a single yaml block, duplicate keys are an
  error**, not a "last wins" silent override. (Matches
  YAML's own duplicate-key strictness when enabled.)

This makes the multi-readout binding capability safe to
use without becoming a footgun: composition is opt-in via
the list form (or `+` prefix for extension), and
replacement is loud about what it replaced.

### 5.5 Composition: events × readouts

A workload's display configuration is a **sum type** —
each event slot is one summand, each summand evaluates to
a (possibly empty) sequence of readout invocations, and
the run-time output is the temporal interleaving of every
summand's renders. The yaml binding picks which summand
contributes what.

The cross-matrix below shows the slot × invocation product
for a representative workload (one `for_each` with three
phases per iteration). The cells show *what renders at
that event for that binding*, in declaration order; cells
read top-to-bottom-left-to-right give the actual stream.

| event slot         | binding A: defaults only | binding B: `on_phase_end` overridden | binding C: custom on_update + custom scope_header |
|--------------------|--------------------------|--------------------------------------|----------------------------------------------------|
| `on_session_start` | (built-in) `session_banner` | same | same |
| `on_each_start`    | (built-in) `scope_header` | same | `my_scope_header` |
| `on_phase_start`   | (built-in, suppressed)   | same | same |
| `on_update`        | (built-in) `phase_status` | same | `my_status` |
| `on_phase_end`     | (built-in) `phase_done`  | `phase_done_quiet` | (built-in) `phase_done` |
| `on_each_end`      | (built-in, empty)        | same | same |
| `on_session_end`   | (built-in) `session_summary` | same | same |

The resulting stream for one iteration of `binding C`
reads:

```
session_banner            ← on_session_start (once)
my_scope_header           ← on_each_start (each iteration)
my_status                 ← on_update (× many, refresh)
my_status                 ← on_update (final, retained)
phase_done                ← on_phase_end
my_status …               ← on_update for next phase…
…
session_summary           ← on_session_end (once)
```

Two properties this exposes:

- **Filtering:** Slots with empty bindings produce nothing
  — that's how surfaces opt out of structural events
  (e.g. a piped log sink binding nothing to
  `on_each_start`).
- **Composition:** Multiple readouts can bind to one slot
  by declaring a list. The engine emits each in order at
  every fire of that event:

  ```yaml
  readouts:
    on_phase_end:
      - phase_done            # the ✓ summary
      - phase_failure_hint    # only renders when errors > 0
  ```

  Conditional rendering is the readout's job, not the
  binding's: a readout that has nothing to say at this
  context emits zero bytes. The composite stream is
  therefore predictable from the binding alone.

This composability is what makes the workload-level
display config a real configuration surface rather than a
single-axis switch. A workload picks a *set* of readouts
across a *set* of events, and the resulting stream is the
deterministic merge.

---

## 6. Snapshots and session retention

### 6.1 Every render is a snapshot

A render is a **timeless, self-contained snapshot** of what
its readout knew at one moment. Once produced, the rendered
text stands on its own — it does not depend on a still-live
data source, a still-running phase, or a still-active TUI.
A user reading a snapshot a week after the run sees the same
thing the live display showed at that moment.

Two consequences:

1. **No mutating placeholders.** A readout never emits text
   like "..." that's a stand-in for "data not available
   yet"; either the data is present and rendered, or the
   readout omits the slot entirely (with §3.2's overlay
   explaining the omission). Every snapshot reads as a
   complete observation.
2. **No back-references.** A snapshot cannot read fields
   that aren't present at render time. The
   `ReadoutContext` it captured during render is the only
   data source — the snapshot does not re-query later.
   This is what makes "good enough for posterity" tractable.

### 6.2 Final render

What "final" means depends on the event kind (§4.1):

**Lifecycle readouts** — `on_*_start` and `on_*_end`
already fire exactly once. Their single render *is* their
final render. There is no separate "finalize" pass; the
once-fired render is the canonical observation and goes
straight into the snapshot store.

**Refresh readouts** (`on_update`) — fire many times
during the in-flight period of their subject. The engine
guarantees one **final on_update tick** per subject,
synthesized at the subject's `_end` boundary if no
refresh-cadence tick happened to coincide. The subject's
context is the last-known state (counters / metrics /
duration as of the `_end` moment). The retained snapshot
is whichever render came latest, so in practice it's
always the synthesized final tick.

This is the rule that makes the inline status line "stay"
after a phase completes: the same `phase_status` readout,
one last refresh fired against the phase's final context,
retained until session close.

**Subject scope.** Every readout has a subject — the
thing it renders *about*:

- A `phase_status` / `phase_done` readout's subject is the
  **phase**; final render at `on_phase_end`.
- A `scope_header` readout's subject is the **iteration
  tuple** (one per `on_each_*` cycle); final render at
  `on_each_end`.
- A `session_banner` / `session_summary` readout's
  subject is the **session** — the entire nmbrs run as a
  single unit. "Session" here matches SRD-45's session
  definition: one resolved session directory, one
  `metrics.db`, one continuous span from invocation
  through (resume × N) to close. A session-scoped readout
  has access to cross-phase totals via `ReadoutContext`
  (total ops, total errors, total elapsed, phases
  completed / failed). The final render of an
  `on_update` readout bound at session scope (rare but
  legal — e.g. a top-of-screen aggregate counter) fires
  at `on_session_end`.

The engine never re-invokes a readout after its final
render. The snapshot is the authoritative artifact from
that point on.

### 6.3 Session-scoped retention

The most recent render of every readout — keyed by
`(slot, subject)` where subject is the phase / scope /
session it belongs to — is **retained in the session**
until the session closes. The retention store lives
alongside `metrics.db` in the session directory, persists
across nmbrs invocations within the session (resumes,
re-renders, post-run inspection), and is dropped on session
delete.

What gets stored: the rendered text (ANSI + plain
fallback) plus the readout's identity and the LOD it was
rendered at. **Not the raw context.** Re-rendering a
different LOD is not supported from the snapshot; if
that's wanted, re-run from `metrics.db`.

> **Deferred to a separate SRD: context freeze for
> diagnostics.** A natural next question is whether to
> *also* persist a copied form of the `ReadoutContext`
> at every lifecycle close — a programmatic /
> expression-queryable artifact ("show me phases where
> p99_ms > 100 and recall_at_10 < 0.95"). This SRD
> deliberately scopes the snapshot to **rendered text
> only**: the storage shape, retention semantics, and
> sufficiency for the live-display use case all hinge on
> that scope. A context-freeze artifact has different
> serialization needs (typed columns, query ergonomics,
> indexing), overlaps with `metrics.db`'s territory
> (which already holds time-series counter / histogram
> data), and warrants its own decision document.
> **Recommendation:** spin out a separate SRD when this
> need becomes concrete; the readout snapshot store and
> the context-freeze store will be sibling artifacts in
> the session directory, not the same artifact.

What this enables:

- **Scrollback in the TUI.** The user pages back through
  completed phases and sees each phase's last live status
  as it stood at completion, not a recomputed view.
- **`nmbrs replay`** (out of scope for first push). Read
  the snapshot store and reproduce the live display after
  the run.
- **`nmbrs report`** can include the final ✓ summary lines
  verbatim, identical bytes to what the operator saw.

### 6.4 Storage shape

Sketch (open for refinement in §11):

```sql
CREATE TABLE readout_snapshots (
    slot TEXT NOT NULL,            -- "on_update", "on_phase_end", ...
    subject_kind TEXT NOT NULL,    -- "phase", "scope", "session"
    subject_id TEXT NOT NULL,      -- phase identity / scope path / "session"
    readout_name TEXT NOT NULL,    -- "phase_status", "phase_done", ...
    lod TEXT NOT NULL,             -- "compact" | "labeled" | "expanded"
    rendered_at INTEGER NOT NULL,  -- nanos since epoch
    body_ansi BLOB,                -- styled rendering (color)
    body_plain TEXT NOT NULL,      -- ANSI-stripped fallback
    PRIMARY KEY (slot, subject_kind, subject_id, readout_name, lod)
);
```

Insert-or-replace by primary key; only the latest render
per `(slot, subject, readout, lod)` survives. Memory
footprint: a few KB per phase × N phases — bounded by the
scenario tree size.

---

## 7. The `ReadoutBinder` — runtime wiring & interaction

A workload's `readouts:` block is a static configuration.
A real run needs *runtime wiring*: who decides which
readouts fire at this event, what LOD each renders at,
whether the overlay's on, where on the line each one goes,
and how operator keystrokes (in the TUI) update any of
those decisions. That is the **`ReadoutBinder`**.

The `ReadoutBinder` is a named, **stateful** trait — the
runtime adapter between the static binding and the live
display surface. One instance per surface (terminal-mode
gets a stateless binder; the TUI gets a stateful one with
focus / LOD overrides / overlay flag). Readouts stay
stateless; the binder owns the per-session interactive
state.

### 7.1 Trait surface

```rust
pub trait ReadoutBinder: Send {
    /// Drive every readout bound to this event. The binder
    /// applies its interactive state (focus highlight, LOD
    /// overrides, overlay-held flag) and emits an ordered
    /// stream of layout-aware render instructions to `sink`.
    fn fire(
        &mut self,
        event: Event,
        ctx: &dyn ReadoutContext,
        sink: &mut dyn ReadoutSink,
    );

    /// Optional: keyboard-event hook for interactive
    /// surfaces. Default no-op for sinks that don't run
    /// in raw mode (terminal log, piped sinks, web).
    fn on_key(&mut self, _key: BinderKey) {}
}

/// What a binder writes into. Layout-aware: the binder
/// signals "this render owns its own line(s)" with
/// `line_break` between groups so the sink composes the
/// final text per the rules in §7.4.
pub trait ReadoutSink {
    fn literal(&mut self, s: &str);
    fn render(
        &mut self,
        readout: ReadoutHandle,        // Arc<dyn Readout>, not &dyn Readout
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        options: &ReadoutOptions,
        layout: LayoutHint,
    );
    fn line_break(&mut self);
}

// As built in `crates/nmbrs-runtime/src/readouts/binder.rs` — `literal()` for
// raw passthrough, `render()` takes an owned `ReadoutHandle`.

pub enum LayoutHint {
    /// Ok to share a line with adjacent compact readouts.
    InlineCompact,
    /// Owns its own line(s); the sink line-breaks before
    /// and after.
    Block,
    /// Highlighted by the binder's focus state — the sink
    /// applies an offset / background-tint per its
    /// surface's conventions.
    Focused(Box<LayoutHint>),
}

pub enum BinderKey {
    /// Cycle which readout in the current event slot is
    /// "selected" / focused.
    CycleFocusNext,
    CycleFocusPrev,
    /// Cycle the LOD of the focused readout
    /// (`compact → labeled → expanded → compact`).
    CycleLodUp,
    CycleLodDown,
    /// Held-key: while pressed, every render fires with
    /// `ContentMode::Explanation`. Released → `Value`.
    OverlayHeld(bool),
}
```

### 7.2 What the binder owns

State carried across event fires:

- **Focus index** per slot — which readout in the slot's
  bound list is currently selected. Used by the focus
  highlight (`LayoutHint::Focused`) and as the target of
  LOD-cycle keystrokes.
- **Per-(slot, readout) LOD overrides** — a small map.
  Empty by default; a user-driven cycle stamps an entry,
  reset clears it.
- **Overlay-held flag** — transient `bool`. While true,
  every fire passes `ContentMode::Explanation` instead
  of `Value`. Cleared on key-up.
- **Composition state** — the per-line grouping rules
  from §7.4. The binder doesn't own a buffer; it just
  decides where line breaks go and lets the sink lay
  out the bytes.

Behaviour the binder runs:

- Resolve `event` against the workload's compiled
  binding → ordered list of (readout, default LOD,
  default mode, options).
- Apply per-(slot, readout) LOD overrides on top of the
  defaults.
- If the overlay-held flag is set, replace mode →
  `Explanation`.
- Walk the list, classify each readout's effective
  rendering as `InlineCompact` or `Block` per §7.4,
  emit `LayoutHint` accordingly.
- Mark the focused entry with `LayoutHint::Focused(…)`.

### 7.3 Interactive cycling — the keyboard contract

Three operations the user can perform at runtime, all
mediated by the binder:

| Keystroke (TUI default) | Binder action                                           |
|-------------------------|---------------------------------------------------------|
| `Tab` / `Shift-Tab`     | `BinderKey::CycleFocusNext` / `…Prev` — move the
  focus highlight to the next/previous readout in the
  active slot. Visual: the focused readout renders with
  its surface's "focused" decoration (offset, background
  tint).                                                                                |
| `+` / `-`               | `BinderKey::CycleLodUp` / `…Down` — change the
  focused readout's LOD by one step.
  `compact → labeled → expanded → compact` (cycle).
  Stamps an override on `(slot, readout)`. Visual:
  takes effect on the next fire of that slot.                                                                              |
| `?` (held)              | `BinderKey::OverlayHeld(true)` on key-down,
  `…(false)` on key-up. While held, every render is
  the explanation overlay. The same readout, same LOD,
  same width — see §3.2.                                                              |

The binder is the *only* place these decisions live. A
readout has no idea it's being focused or LOD-cycled;
the surface has no idea what events are bound where.
Both stay simple by talking only to the binder.

The TUI surface owns the keyboard event loop and forwards
to its `ReadoutBinder` instance via `on_key`. Non-
interactive surfaces (terminal-mode log, piped sinks,
inspector) implement a binder that ignores `on_key` and
runs with no interactive state at all.

### 7.4 Layout composition rules

Two basic rules drive how the binder lays out a list of
readouts on a surface, with an explicit per-call
override:

1. **Compact forms can share a line.** Multiple readouts
   that render at `Lod::Compact` for `Value` mode
   produce single-token output (§3.1). The binder
   concatenates consecutive compact readouts, separated
   by a single space, onto one line.
2. **Multi-line forms (Labeled / Expanded) are
   block-rendered.** The binder line-breaks before and
   after a non-compact readout; the readout owns the
   line(s) it occupies.
3. **`layout=` option overrides the LOD classification.**
   See §5.3.1. `layout=inline` forces
   `LayoutHint::InlineCompact` even at non-compact LODs;
   `layout=block` forces `LayoutHint::Block` even at
   compact LODs; `layout=auto` (default) picks per LOD.

The decision tree the binder runs per readout in
the resolved list:

```text
layout = explicit layout= option (default = auto)

if layout == inline:
    hint = InlineCompact
elif layout == block:
    hint = Block
else:  # auto
    if lod == Compact and mode == Value:
        hint = InlineCompact
    else:
        hint = Block
```

Pseudocode for the `fire` walk:

```text
for r in resolved:
    if r.lod == Compact and r.mode == Value:
        # share a line
        if previous_was_block: sink.line_break()
        sink.render(r, …, LayoutHint::InlineCompact);
    else:
        # block
        sink.line_break()
        sink.render(r, …, LayoutHint::Block)
        sink.line_break()
        previous_was_block = true
```

The "compact composes, multi-line breaks" invariant is
what makes mixed-LOD bindings (`on_update: [throughput,
latency_block]` where `throughput` is compact and
`latency_block` is expanded) lay out predictably:
`throughput` shares a line with whatever neighbouring
compact readouts the surface schedules; `latency_block`
takes a clean block of its own below.

`LayoutHint::Focused` wraps either of the two basic
hints — a focused compact readout still shares lines (the
focus decoration is applied within its slot), and a
focused block readout still gets its own line.

### 7.5 Compilation interaction

The render-step list (§8) is per-readout, owned by the
readout itself. The binder doesn't see render steps; it
only schedules which readouts run. So the binder's state
(focus, LOD overrides, overlay flag) has no effect on
the bake artifact — those are runtime parameters fed
into the readout's `render()` call. A binder that's been
overlay-toggling all session long doesn't invalidate any
baked readout.

For the focus highlight specifically, the binder's
state changes only the `LayoutHint` it emits — the
readout's own rendering bytes are unaffected. The sink
is what applies the offset / background tint, with no
involvement from the readout. This keeps readouts
oblivious to whether they're "selected" and lets the
surface decide the visual convention.

---

## 8. CLI override: `--readout`

A single CLI flag picks the `on_phase` readout for the
current run:

```
nmbrs run workload=… --readout=phase_done
nmbrs run workload=… --readout=compact
nmbrs run workload=… --readout="lod:compact phase_done"
```

If the value is a known built-in name, that readout
applies. If it's a body string, it parses inline. The flag
overrides whatever the workload declared.

Per-event flags (`--readout-on-each=…`, etc.) are out of
scope for the first push; the single `--readout` is the
escape hatch for running an existing workload with a
different status format from one shell.

---

## 9. Compilation pipeline

Readouts compile **once** at workload load:

1. **Parse.** Lex each readout body into literal runs and
   `{…}` bind expressions. Total grammar; no late binding,
   no string-templating fallbacks.
2. **Bake.** Resolve every `{name}` against the readout
   registry. Validate options. Emit a `Vec<RenderStep>`
   where each step is `Literal(&str)` or `Render(readout,
   options)`. The literals borrow from the parsed body.
3. **Render.** Hot-path walks the step list:
   ```rust
   for step in &baked.steps {
       match step {
           RenderStep::Literal(s) => out.write_str(s),
           RenderStep::Render(r, opts) =>
               r.render(ctx, lod, opts, out),
       }
   }
   ```
   No per-render allocation beyond `out`. No name lookup.
   No options re-parsing.

A baked readout is cheap to clone (Arc) and shared across
phases. A workload that runs 200 phases with the same
readout pays the parse + bake cost once.

---

## 10. Layout vs content

> **Layout** = where the readout sits on screen — gutters,
> tree branches, panel borders, indentation, alignment
> columns.
> **Content** = the readout's rendered text.

Readouts produce **content only**. The call site owns the
**shell** — indent, gutter, panel frame, ANSI stripping for
non-TTY paths — and asks the readout for the content to
drop in.

`ReadoutContext` carries structural info
(`nesting_depth()`, `scope_chain()`) so a readout can
*reflect* its position in its content (e.g. structural
readouts at `on_each` events) without owning the
indentation. Width budgets pass through `ReadoutContext`
too: readouts that auto-truncate (long coords at narrow
widths) do so without the call site re-measuring.

The same compiled readout drives:
- a terminal-mode log line (no shell),
- a TUI tree row (shell = branch glyph),
- a TUI panel body (shell = padding inside frame),
- a piped sink (shell = timestamp prefix, ANSI stripped).

---

## 11. Phasing

*As-built (2026-06-20): Pushes 1–3 plus the stateful binder
(originally slated "Push 5") have LANDED. The remaining open work is
Push 4 — explanation overlays.*

**Push 1 — `on_phase` only.** Readout trait, registry,
`ReadoutContext` facade, baked-step renderer. Built-ins:
`phase_done`. Wire `nmbrs-runtime::activity`'s ✓ DONE +
inline progress lines through the engine. Workload
`readouts:` parses and validates but only `on_phase` slot
is wired. Output byte-equivalent to today's.

**Push 2 — `on_each` + `scope_header`.** Move the scope
ancestor walker (`log_only_observer::phase_starting`) onto
the engine. First multi-event surface.

**Push 3 — TUI surfaces.** Active-phase panel header +
detail block + scenario tree row through readouts.
`expanded` LOD lands here (sparkline + latency block as
expanded readouts).

**Push 4 — explanation overlays.** Toggle binding,
overlay rendering, all built-ins ship overlays. Deferred
because it's UX work and the engine should bed in first.

**Push 5 — `--readout` CLI flag, post-run summary,
inspector / web sinks.**

---

## 12. Resolved design decisions

These were the open items in earlier drafts. Folded here
for traceability — the body of the SRD reflects each
decision in its proper section.

1. **No shorthand forms; the `metric` readout is
   spec-driven.** There are no specialized
   per-metric-name readouts (no `latency_p99` as its own
   type). Every live metric is reached through the
   parameterized `metric` readout with an explicit
   pattern (`metric pattern=…` or the colon sugar
   `metric:…`). The `metric` readout's data source is
   `LiveMetricSource` (§2); whatever names that source
   exposes are the spec — readouts don't enumerate or
   alias those names. **Body home:** §4.2, §5.1.

2. **Workload-level binding only for the first push.**
   The `readouts:` block lives at the workload root.
   Per-phase overrides are not in scope; if a workload
   needs different displays per phase, a future revision
   can add `phases.<name>.readouts:` with the same
   grammar. No mechanism in this SRD presumes per-phase
   binding. **Body home:** §5.

3. **Structured-data emit bypasses readouts.** The
   inspector socket, JSON sinks, and any other
   programmatic consumer reads `ReadoutContext` directly.
   Readouts are for human-rendered output only; they do
   not double-duty as field extractors. **Body home:**
   §2 (open-Q callout updated).

4. **Width overflow is line-break, not truncate.** When
   `labeled` LOD content exceeds the available width,
   the engine wraps to additional lines rather than
   asking readouts to self-truncate. Each readout's
   render is therefore deterministic at every width;
   the surface owns the wrap. **Body home:** §3 (LOD
   table footnote), §9.

5. **Explanation overlay is a content mode on the same
   readout.** Not a sibling readout, not a separate
   trait method — the existing `Readout::render()` takes
   a `mode: ContentMode` parameter alongside `lod`, and
   the overlay text is produced by the same impl with
   the mode flipped. The overlay shares context, LOD,
   width budget, and styling rules with the value
   render. **Body home:** §1 (trait update), §3.2.

---

## 13. References

- SRD 41 §"Logging and Diagnostics" — observer event surface
  the engine renders into.
- SRD 62 §"LOD controls govern phase expansion" — the
  surface-level density control that picks readout LOD.
- SRD 46 — the `report:` block (separate concern; this SRD
  intentionally avoids the word "template" to keep the two
  surfaces distinct).
- `feedback_no_presumed_features` — every workload-specific
  emphasis is opt-in.
- `feedback_never_ignore_silently` — unknown readout names
  / options are load-time errors.
