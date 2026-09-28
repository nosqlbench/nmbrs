# 62: TUI Layout

The TUI is a live view of a running scenario. This document
sketches the layout model the current display is migrating
toward. The north stars: **phases are not singletons**, **the
scenario tree is the primary organizing structure**, and **a
120-column terminal is the baseline width**.

---

## What changed in the mental model

### Phases aren't global

Today's TUI renders a dedicated "Phase" panel at the top of the
screen showing _the_ running phase — name, cursor, fibers,
progress bar, running counters. This implicitly assumes a
scenario has exactly one phase in flight at any moment.

That assumption is wrong. A scenario can have multiple phases
running concurrently: stanza-level parallelism, multi-activity
session layouts, or (in future) named phases driven off
independent sources. The display must scale from 1 to N
concurrent phases without a layout change.

**Consequence:** there is no global phase panel. Every piece
of information that used to live in the global "Phase" panel
— cursor identity, op template count, progress bar, rates,
latency percentiles, relevancy scores, adapter counters,
throughput sparkline — is a property of a specific phase and
belongs in that phase's row (and its expanded detail) within
the scenario tree.

### The scenario tree is the primary panel

The scenario tree stops being a sidebar and becomes the main
canvas. Every phase — pending, running, or completed — is a
row in the tree. Expanded phase rows own the full set of
live/retrospective metrics for that phase, rendered inline
under the phase name:

- progress bar + cursor identity + active/pending/complete
- cycle/cursor rate, error rate, concurrency
- adapter-declared counters with rates (rows_inserted/s, …)
- relevancy metrics (recall@k, precision@k, …)
- latency percentiles (min / p50 / p90 / p99 / p999 / max),
  rendered as the same per-row bar visualization we use today
- throughput sparkline (ops/s + primary adapter counter/s)

Each of these is computed against that phase's label set via
`MetricsQuery::active_phase_selection(...)`, not over the
session globally. The existing per-panel renderers don't
disappear — they get called per-phase, parameterized by that
phase's selection, laid out vertically in a phase's detail
block.

**There is no global latency view and no global throughput
view.** Each phase has its own latency block and throughput
sparkline inside its detail section. Overlays attached to
those views (past-span pinning, cross-bar peak markers,
alternate percentile/timeseries renderings) are properties
of *that phase's* block, not of a top-level panel. The
question "which phase does this overlay apply to?" always
has exactly one answer: the phase whose detail block it lives
inside.

### LOD controls govern phase expansion

The scenario tree already has a Level-of-Detail control
(left / right arrows). Under the new model, LOD has a stronger
role: it decides how many phases are visible and how much of
each phase's detail is expanded. Proposed LOD levels:

| LOD      | Scope                                            | Phase detail           |
|----------|--------------------------------------------------|------------------------|
| Hidden   | tree replaced by log panel                       | —                      |
| Minimal  | phases as one-line entries, no scope headers     | collapsed              |
| Default  | phases + scope headers                           | selected phase expanded|
| Focus    | only the active phase(s), scope hidden           | always expanded        |
| Maximal  | every entry                                      | always expanded        |

`Focus` is the new mode. It's the answer to "I'm running one
phase and I want the screen to act like a status dashboard for
it." When exactly one phase is running, Focus renders that
phase's full detail in the canvas — progress bar, throughput,
latency, adapter counters, relevancy — and nothing else. It
looks like a global status panel.

**It isn't one.** If a second phase starts while Focus is
active, the LOD auto-widens: both running phases render, each
with its own detail block stacked vertically. When the scenario
drops back to one running phase, Focus re-tightens. The user
never has to pick "global vs per-phase" — the display adapts
to whatever concurrency the scenario actually exhibits.

Auto-widening only fires in Focus. If the user has pinned a
different LOD (Default, Maximal, Minimal, Hidden), concurrency
changes don't re-pin them.

### 120-column baseline

The layout is designed for a 120-column-wide terminal. At
120 columns every row fits without wrapping or re-layout:

- tree markers + indent + phase name + dimensional labels
- latency percentile row: label + value + bar + both range
  endpoints
- throughput sparkline: label + spark + current value
- progress bar + counts

Wider terminals add right-side padding — they do not pack
additional content into each row, and they do not trigger a
multi-column reflow. This keeps the design one shape and one
code path. Narrower terminals are permitted to degrade
gracefully (truncation at the right edge) but are not an
explicit design target.

Concretely, the width budget for a phase detail row at 120
columns is:

```
│   [status]  phase_name (labels)                          … padding …  │
│   │                                                                    │
│   │ progress   ━━━━━━━━━━━━╌╌╌   <N> of <M>   (<P>%)                  │
│   │ cursor     <name>/s: <rate>   ok: <pct>%   err: <n>   ret: <n>    │
│   │ adapter    <k1>:<v1>@<r1>/s   <k2>:<v2>@<r2>/s                    │
│   │ relevancy  recall@10: <v>     precision@10: <v>                   │
│   │ latency    min <m>..max <M>   ━━━━━━━━━╬╌╌╌                       │
│   │            min <v> ━━━━━━╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌                │
│   │            p50 <v> ━━━━━━━━╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌                │
│   │            p99 <v> ━━━━━━━━━━━━━╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌                │
│   │            max <v> ━━━━━━━━━━━━━━━━━━━━━╌╌╌╌╌╌╌╌╌╌                │
│   │ rate       ━━━━━━━━━━━━━━━━━━━━━━━━▇▆▅▃▁▁▁▁▁▁▁▁▁   <cur>/s       │
```

Each row fits in ≤120 columns with the standard tree indent
(`marker + per-depth indent + pipe gutter`). Phase detail
rows are right-padded, not re-flowed.

---

## Panel composition

The new screen is a vertical stack of fewer, larger panels:

```
┌─ Header ────────────────────────────────────────────────┐
│ session, elapsed, paused banner                          │
├─ Log (optional) ─────────────────────────────────────────┤
│ severity-colored messages, toggled with `l`              │
├─ Scenario ──────────────────────────────────────────────┤
│ tree rows + per-phase expanded details                   │
├─ Footer ─────────────────────────────────────────────────┤
│ key hints                                                │
└──────────────────────────────────────────────────────────┘
```

Compared to the current layout, the **Phase**, **Latency**,
and **Throughput** panels are gone as global chrome. Their
renderers are now called per-phase inside the tree's detail
blocks. The Log panel stays as an optional sibling because
its content (session-level log messages) really is global.

When Hidden LOD is active and the log is on, the tree's
section disappears entirely and the log takes its place —
which is already the behavior today and remains the rule.

---

## Migration notes

- `draw_phase`, `draw_latency_*`, and `draw_sparklines` keep
  their rendering code but move from top-level `draw()` calls
  to helpers invoked per-phase inside the tree's detail loop.
  Each takes a `Selection` (the phase's dimensional labels)
  and a `MetricsQuery` handle; there is no global-scope
  `MetricsQuery` read path for display.
- The "active phase" pointer in `RunState` (`state.active`)
  becomes a list (`state.active_phases`), since multiple
  phases can be running. Observer callbacks already key their
  updates by `(name, labels)` — the TUI-side collection
  changes from `Option<ActivePhase>` to
  `HashMap<(String, String), ActivePhase>`.
- `TreeLod` gains a `Focus` variant. The `left`/`right`
  arrow cycle becomes `Hidden → Minimal → Focus → Default →
  Maximal`. Focus is a dynamic mode: its effective expansion
  set is re-computed each frame from the current running-phase
  set, not stored in the LOD value itself.
- Sparkline history continues to be scoped per-phase (already
  cleared on phase boundaries) — it now lives inside each
  phase's `ActivePhase` instance instead of a single global
  history ring.
- Key bindings stay the same. The only new key behavior is
  `Focus` appearing in the LOD cycle; no new standalone key.

---

## Design notes

### Per-phase sparkline: binomial-reduction summary

Per-phase sparkline storage is not a UI-side ring buffer — it's
a **summary** (in the sense of
[`nmbrs_metrics::summaries`](../../crates/nmbrs-metrics/src/summaries/mod.rs)):
a retained, transforming view of instrument data, attachable to
any scalar-valued metric (rate, gauge, counter) the same way
other summaries like `F64Stats` and `LiveWindowHistogram` attach
today.

- **Bucket count is supplied by the caller** as the display
  horizontal resolution. In TUI mode this is the column
  width allocated to the sparkline — one sample per column
  for block-char sparklines, two samples per column for
  braille-rendered ones.
- **Binomial reduction on fill.** When the buffer reaches its
  configured bucket count, it halves its effective resolution
  by pairwise-merging adjacent buckets. Two buckets combine
  into one that represents their average (or appropriate
  reduction for the scalar kind), freeing room for new samples
  at the leading edge. The buffer never exceeds its bucket
  cap; it just holds a coarser view of the same window.
- **Lifecycle attachment.** The summary is attached at phase
  start as a consumer of the phase's chosen scalar (ops/s,
  rows_inserted/s, etc.) and detached at phase end. Samples
  accumulate only while the source publishes; once the phase
  stops, the instrument receives no further values and the
  rendered sparkline is frozen. This is the durable artifact
  that remains visible after phase completion and lets the
  user scroll back through a completed phase and still see
  its throughput shape.

This replaces the ad-hoc "clear on phase boundary, cap at
120" ring buffer the TUI uses today. The cap is a property
of the instrument's bucket count, not of a display data
structure, so the same summary can drive Shift+P captures,
report artifacts, or an offline replay without the TUI being
in the loop.

### Phase status glyphs and the running spinner

Every phase row in the scenario tree carries a single-character
status glyph — the redundant-encoding partner to its name's
health color. The glyph is read first by colorblind users and
last (as a confirming signal) by everyone else.

| Status | Glyph | Color rule |
|--------|-------|-----------|
| Pending | `○` | dim |
| Running | animated Braille spinner (see below) | health tint |
| Completed | `✓` | health tint (green / orange / red by error rate) |
| Failed   | `✗` | hard red |

The Running glyph is an animated rotation drawn from
`throbber_widgets_tui::symbols::throbber::BRAILLE_SIX` rather
than a static `▶`. Two properties matter:

- **4 Hz cadence, wall-clock-driven.** The rendered frame is
  `((SystemTime::now() / 250 ms) as usize) % N` — derived from
  the wall clock, not from a per-renderer counter. This means
  a frame change always appears at every tick (the TUI redraws
  at 4 Hz), not on a quarter of them.
- **Lockstep across concurrent phases.** Because the index is
  derived from the same global clock, every Running phase
  shows the *same* spinner frame on every render. The eye
  reads the rotation as a single unified "the system is
  working" cue rather than several phases stuttering against
  each other on different cycles.

`BRAILLE_SIX` was chosen for its dense rotation feel and its
graceful degradation: on partial-glyph fonts it still renders
as recognizable dots rather than a missing-character box.

A helper `spinner_frame() -> &'static str` in `nmbrs-tui::app`
encapsulates this — there is no spinner state on `App` or
`RunState`; the function is pure over `SystemTime::now()`.

### "Scenario done?" is a component-tree query

The waiting-vs-done distinction under Focus LOD is not a
custom traversal over `RunState.phases`. It's a structural
query on the activity runner component, which is already in
scope for the TUI via the component tree / MetricsQuery
closure the runner passes in. Concretely:

- Scenario is **done** when the activity runner component
  reports no running children and no pre-mapped children
  that have not reached running status. The TUI renders a
  "scenario complete" banner and exits shortly after.
- Scenario is **waiting** when there are pre-mapped children
  still in a pending state but none currently running. Focus
  shows a "waiting for phase…" placeholder.
- Scenario is **live** when at least one phase is running.
  Focus renders that phase's detail block (or all running
  phases, if multiple).

Keeping this query on the component tree — rather than
re-implementing it over the TUI's `phases` vec — means the
TUI inherits whatever the component model decides about
pre-mapping, abort, and partial-failure semantics without
duplicating that logic.

---

## Open questions

*(none outstanding — all design points above are committed.)*

---

## Non-goals

- A responsive multi-column layout for wider terminals. One
  layout, padded on wide screens, degraded on narrow.
- A full-screen single-phase view with no scenario tree. Focus
  LOD covers this need without splitting the code paths.
- Live reordering of phases in the tree. The tree renders in
  declaration order; for_each iterations extend it in discovery
  order. That's stable and predictable.
