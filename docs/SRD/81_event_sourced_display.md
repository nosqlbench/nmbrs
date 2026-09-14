# SRD-81: Event-Sourced Display Projections

> **Status:** DRAFT — **model agreed** (all four §11 decisions
> resolved, 2026-06-07); ready to cut push 1. Names the **projection
> model** for all display surfaces and the single **`ReadoutSink`
> seam** as the load-bearing artifacts. Much of the seam already
> exists (`StringSink` + `TuiReadoutSink`); the canonical durable
> record already exists (the per-phase session store); the core work
> is the de-conflation + typing the events ring (push 1).
>
> **As-built (2026-06-20):**
> - The §8 de-conflation LANDED: the old
>   `diag!(Info, phase_outcome.render())` is GONE; the phase-end
>   outcome now flows through
>   `observer::log_categorized(LogLevel::Info, LogCategory::PhaseOutcome, &rendered)`
>   (`nmbrs-runtime/src/activity.rs` ~line 2560), so the TUI log panel
>   filters it and re-projects natively via `TuiReadoutSink`.
> - The durable canonical record is BUILT: checkpoint JSONL
>   (`CheckpointData` with `event_type() -> Option<EventType>`) +
>   sqlite (`phase_outcomes`, `phase_errors`, `readout_snapshots`);
>   `nmbrs replay` and `nmbrs checkpoint show|fold` re-project from it.
> - REMAINING (§7): the in-memory ring `RunState.log_messages` is still
>   `Vec<LogEntry{ severity, message: String, category: LogCategory }>`
>   — i.e. category-tagged RENDERED STRINGS, NOT the fully-typed
>   `PhaseEnd{summary}` payload §7 calls for. So today's de-conflation
>   is by the CATEGORY tag; the typed-payload events ring is the open
>   work.

## 1. Ownership & relationships

This SRD owns **how run state and run events reach a display
surface** — the data-flow contract, not the panels or the readout
content. It sits across:

- [SRD-63 readouts](63_status_readouts.md) — owns *what* a readout renders
  and the `ReadoutSink` / binder mechanism this SRD generalizes.
- [SRD-76 phase outcome](76_phase_outcome_disposition.md) — owns the
  structured `PhaseOutcome`/`PhaseSummary` this SRD projects instead
  of re-deriving.
- [SRD-44a checkpoint event log](44a_checkpoint_jsonl.md) — owns
  the durable JSONL `CheckpointData` stream + fold tool; this SRD
  treats it as one persisted projection of the spine.
- [SRD-62 TUI layout](62_tui_layout.md) — owns the panel geometry
  this SRD feeds.
- [SRD-02 concurrency](02_concurrency_model.md) +
  `design/tui_status_display.md` — own the actor + ArcSwap
  decoupling this SRD's spine flows through.

The forcing question: **can every display surface be expressed as a
pure projection of one typed event stream (plus the snapshot that is
itself a fold of that stream), so that no surface ever consumes a
pre-rendered string produced for another surface?** This SRD says
yes, and shows the seam.

## 2. The defect this fixes

The per-phase `✓` outcome is rendered *once* by the SRD-63 binder
into a terminal-flavored ANSI string (`StringSink`), then pushed
into the **log plane** as an `Info` `diag!` (`activity.rs`
on-phase-end, SRD-63 Push 1). A *render* is masquerading as a *log
message*. Consequences:

- The TUI `draw_log` panel renders it as one un-split, un-stripped
  `Span` → garbled (`\n` + raw escapes shown literally).
- `session.log` interleaves ANSI multi-line render blocks with real
  diagnostics.
- It fills the 200-entry `log_messages` ring with multi-line blocks.
- The terminal "scrollback is the home for phase history" decision
  (the managed region removal, `design/tui_idempotent_phase_history_repaint.md`)
  silently depends on this conflation.

Meanwhile the *structured* form of the same fact already exists in
the fold (`RunState.phases[i].summary: PhaseSummary`) and the
durable log (`CheckpointData`). We render-once-as-string instead of
projecting-per-surface.

## 3. The model

Three roles, one direction of flow:

```
   domain events  ──▶  EVENT STREAM (typed, ordered)
   (lifecycle,           │
    diagnostics)         ├─ fold ────▶ scene tree / RunState snapshot
                         │             (live canonical state; ArcSwap)
                         │
                         ├─ persist ─▶ SESSION STORE (durable canonical record):
                         │             sqlite phase_outcomes + metrics tables
                         │             + checkpoint JSONL — written per phase
                         │
                         └─ project ─▶ surfaces, each via a ReadoutSink:
                                       terminal scrollback (StringSink)
                                       TUI tree / panels   (TuiReadoutSink)
                                       session.log         (StringSink, plain)
                                       replay (re-project from the session store)
```

- **Spine — one typed, ordered event stream.** Entries are typed
  facts, never pre-rendered strings (§4). Its **durable, complete
  form is the session store** — sqlite `phase_outcomes`/`phase_errors`
  (SRD-76) + the metrics tables + the checkpoint JSONL (SRD-44a) —
  **written per phase on completion** (`executor.rs`
  `write_phase_outcome`, deliberately *before* the scene-tree
  mutation so even a panic leaves a durable row). Its **in-memory
  form is a bounded ring** (`LOG_RING_CAPACITY`) — a *live-display
  render cache*, NOT the source of truth.
- **Fold — the scene tree / `RunState` snapshot.** The current state
  is a fold of the stream (`phases` + `PhaseOutcome`, scope tree) —
  the *live* canonical state (in-memory, ArcSwap), of which the
  session store is the durable persist. Surfaces that show *live*
  state (the status line, the TUI tree) project the fold; *history*
  surfaces (terminal scrollback) project the recent ring; **replay**
  re-projects from the session store.
- **Projection — every surface, through one seam.** A surface
  renders by feeding events/fold through a **`ReadoutSink`** (§5) —
  `StringSink` (terminal + `session.log`) or `TuiReadoutSink` (TUI).
  The *same* readout logic renders natively per surface; no surface
  consumes another surface's output.

**Guardrail — only durable facts are events.** Lifecycle
transitions, diagnostics, and control changes are events. The
high-cadence `Update`/metric tick is **not** an event — it is a
*fold projection* re-derived from the snapshot each render tick.
This keeps the event ring and the checkpoint log bounded; the live
status line never enters the stream.

**Metric values live in the metrics store, never in the event
stream.** A lifecycle event — and its durable `phase_outcomes` row —
carries only the *fact* + identity + **window** (`PhaseEnd` = which
phase, what window, terminal status); the recall / latency /
throughput **values** are **pulled from the metrics store on demand**
(SRD-40/42) and rendered through the existing readouts ("the original
way they were already displayed"), only when a surface needs them.
Both halves live in the session store — `phase_outcomes` (the fact +
window) and the metrics tables (the values) — so phase-outcome
display is a **join**: *lifecycle event (which phase, what window) × a
query against the metrics store*, re-derivable identically at realtime
and at replay. Nothing denormalizes metric values into the event, the
snapshot, or a persisted render.

## 4. The event vocabulary

The lifecycle **kind-tag** is unified as **`lifecycle::EventType`**
— the fieldless dispatch selector (`PhaseEnd`, `Update`,
`ScopeStart`, …) with `slot_name()` / `subject_kind()`. It is shared
by the readout binder (which slot to fire) and the durable log:
**`CheckpointData`** (the data-carrying JSONL record) answers
`event_type() -> Option<EventType>`, tying each durable record to
its kind (pre-map / metadata records like `PhaseDeclared` /
`PhaseHash` return `None` — no fire point).

The **spine entries** are data-carrying facts, each either a
`Diagnostic` (severity + text) or a lifecycle fact tagged by its
`EventType`. Payloads:

| Kind | Payload (structured, surface-agnostic) |
|---|---|
| *diagnostic* | `severity`, `text` — `observer::log` / `diag!` |
| `PhaseStart` | phase id, name, seq, labels |
| `PhaseEnd` | phase id + **window** + terminal status (`PhaseStatus`/errors, SRD-76); metric *values* pulled from the metrics store at render, not carried here |
| `EachStart`/`EachEnd` | iteration id, labels |
| `ScopeStart`/`ScopeEnd` | scope id, coords |
| `SessionStart`/`SessionEnd` | session id, disposition |
| `ControlChange` | name, old, new (SRD-23) — proposed new `EventType` variant |

**Resolved (decision 1):** the **kind-tag is unified**
(`lifecycle::EventType`, done) while the **data records stay
separate per concern** — `CheckpointData` for durable recovery
(carries cursor state, `PhaseHash`; no diagnostics), and a
display-spine record for rendering (carries diagnostics; no cursor
state) — both tagged by the same `EventType`. Rationale: diagnostics
must not pollute the recovery log and cursor state must not enter the
display ring; sharing the kind-tag gives the projection layer one
vocabulary to dispatch on without coupling the two data concerns.
Push 1 needs only the *diagnostic* + `PhaseEnd` kinds typed; whether
to physically merge the two records is re-opened in push 3 if a
concrete need appears.

## 5. The load-bearing seam — `ReadoutSink`

`ReadoutSink` (`nmbrs-runtime/src/readouts/binder.rs`) is already the
projection abstraction:

```rust
pub trait ReadoutSink {
    fn literal(&mut self, s: &str);
    fn render(&mut self, readout: ReadoutHandle, ctx: &dyn ReadoutContext,
              lod: Lod, mode: ContentMode, options: &ReadoutOptions,
              layout: LayoutHint);
    fn line_break(&mut self);
}
```

**Two impls already exist** — the per-surface projection model is
half-built and proven:

- **`StringSink`** (`readouts/binder.rs`) — plain text for the
  terminal (`\r\x1b[K…`) and `session.log`.
- **`TuiReadoutSink`** (`nmbrs-tui/src/readout_sink.rs`) — the ratatui
  `Line`/`Span` impl, honoring `LayoutHint` as styled spans. This
  **is the "`SpanSink`"** earlier drafts slated as new work. The TUI
  already renders phase readouts natively through it:
  `readout_panel::render_phase_readouts` fires `EventType::PhaseEnd`
  → `phase_outcome` (terminal) / `EventType::Update` → `phase_status`
  (live) into a `TuiReadoutSink`, mounted in the **tree-expanded
  phase detail block** (`app.rs`) and the **active-phase panel**
  (Focus LOD). So the rich per-phase outcome already has a native
  TUI home that never touches `draw_log` (decision 2).

**The load-bearing rule:** *every* display of readout content goes
through a `ReadoutSink`; a surface NEVER receives a finished string
built for another surface. The binder fires an event **once**; the
fan-out renders it into each active surface's sink (terminal
`StringSink` / TUI `TuiReadoutSink`) plus the persisted-projection
sinks (session.log, replay capture). Replay re-projection (SRD-76
re-runs the binder) is preserved because replay is just another
`StringSink`/`TuiReadoutSink` projection of the same events ×
metrics-store query.

## 6. Per-surface projection contract

- **Terminal (`LogOnlySink`).** Projects the **event stream** to
  scrollback via `StringSink`: `Diagnostic` → a severity-colored
  line; `PhaseEnd` → the rich outcome render (ANSI, multi-line). The
  `resume_from` cursor (already built) is the stream cursor; swap
  re-entry re-projects the events that scrolled under the alt-screen.
  The live status line is a **fold** projection, re-rendered each
  tick (unchanged).
- **Main TUI (`app.rs`).** The **log panel projects `Diagnostic`
  events only** — phase outcomes never reach `draw_log` (today they
  only do via the `diag!` conflation §8 removes). The rich per-phase
  outcome **already** projects natively via `TuiReadoutSink`:
  `render_phase_readouts` (`EventType::PhaseEnd`/`Update`) mounted in
  the **tree-expanded detail block** + **active-phase panel**
  (decision 2 — no new panel). The metric *values* come from the
  metrics store, queried for the phase window (§3), so the tree fold
  needn't carry them. The live status header is a fold projection,
  also through `TuiReadoutSink`.
- **`session.log`.** (decision 3 = A) The **complete** plain-text
  `StringSink` projection of the event stream — diagnostics (all
  severities) **plus** the per-phase outcome rendered *the original
  way* (status + recall + latencies, metric values pulled from the
  metrics store), no ANSI. It is the standalone human-readable "what
  happened" record (`2>`/`tail`/`grep`). The structured/queryable
  form *additionally* lives in sqlite (SRD-76) + the metrics store —
  different consumers, not duplication.
- **Replay (`nmbrs replay`).** Re-projects the persisted events
  through the same `StringSink`/`TuiReadoutSink`, joining each
  lifecycle event against the persisted metrics store — realtime and
  replay use one projection path (extends the SRD-76 contract).

## 7. Typing the events ring

`RunState.log_messages: Vec<LogEntry { severity, message, tag }>`
generalizes to an ordered **events ring** of typed entries. Entries
carry **structured payload**, not `message: String`. The actor
append path (`RunStateCmd::Log` → `push_log_tagged`) generalizes to
typed append; the `resume_from` re-emit walks typed entries and
projects them. The de-conflation (§8) and the typed ring are push 1.

> **Implemented 2026-08-13 — the orthogonal `EventTag`.** The old
> single `LogCategory` enum conflated two axes: `Diagnostic`
> ("anything else") rode beside boundary-attached kinds
> (`PhaseLifecycle`/`PhaseOutcome`/`PhaseDetail`), and sinks keyed
> ad-hoc presentation rules off the conflation. It is replaced by
> `observer::EventTag`, two orthogonal axes:
>
> - `attached: Option<lifecycle::EventType>` — WHICH execution-
>   lifecycle boundary the event belongs to, in the CANONICAL
>   lifecycle vocabulary (session/scope/each/phase start/end);
>   `None` = in-flight. A readout render is tagged with the slot
>   that fired it — the firing event IS the attachment.
> - `category: EventCategory` — WHAT the event is about:
>   `General | Outcome | Evaluation | Retry | Throttle |
>   Resolution | Report` (extended as producers appear; a category
>   earns a variant when a consumer needs to dispatch without
>   string-matching rendered prefixes).
>
> The old bundles decompose exactly: `PhaseLifecycle` =
> boundary-attached ∧ `General`; `PhaseOutcome` = `PhaseEnd` ∧
> `Outcome`; `PhaseDetail` = `PhaseEnd` ∧ `Evaluation`;
> `Diagnostic` = in-flight (now carrying real categories: the
> retry advisories/exemplars are `Retry`, the throttle governor is
> `Throttle`, nearest-first shadowing warnings are `Resolution`).
> Sink rules derive from the axes: the terminal sink hides
> boundary-attached `General` renders from scrollback (the
> phase-history region mirrors them); the TUI log panel shows
> `attached == None`; `completed_phases=headers` folds
> `PhaseEnd ∧ Evaluation` rows with their block.

## 8. De-conflation

Remove the `diag!(Info, phase_outcome.render())` from `activity.rs`
on-phase-end. The phase end becomes a typed `PhaseEnd{identity,
window, status}` event appended to the spine (metric values stay in
the metrics store, §3). Surfaces project it (§6), joining the metrics
store for values. Net: the same fact, one structured event, native
renders per surface, zero pre-rendered strings crossing planes — and
the durable `phase_outcomes` row already written at this point
(`executor.rs`) is the canonical record the projections re-derive
from at replay.

## 9. Pushes (sequenced)

1. **De-conflate + typed events ring (first shippable slice).**
   `LogEntry` → a typed spine record (at least *diagnostic* +
   `PhaseEnd{identity, window, status}`, tagged by
   `lifecycle::EventType`); drop the activity-layer `diag!`-of-render;
   terminal `StringSink` projects `PhaseEnd` (joining the metrics
   store for values); TUI `draw_log` filters to diagnostics;
   `session.log` projects the plain outcome text. No TUI change needed
   — phase outcomes already render natively via `TuiReadoutSink` in
   the tree-expanded / active-phase panel. This alone removes the
   garbling, the `session.log` ANSI, and the ring pressure.
2. **TUI native-projection audit.** The ratatui sink already exists
   (`TuiReadoutSink`) and the tree-expanded detail / active-phase
   panel already project phase readouts through it — so this push is
   mostly *audit + fill gaps* (e.g. confirm scope/session lifecycle
   events project natively, not via any string path), not new
   machinery.
3. **Reconcile persistence + replay.** `session.log` and
   checkpoint/sqlite as named projections; `nmbrs replay` re-projects
   typed spine records; re-open whether to physically merge the
   display-spine record and `CheckpointData` (the kind-tag
   `lifecycle::EventType` is already shared — decision 1).

## 10. Risks & the property test

The **load-bearing test** (cf. SRD-47's reducer-equivalence): a
**sink-agreement property** — for any event, the `StringSink` and
`TuiReadoutSink` projections carry the same semantic content
(text-equal after stripping styling/ANSI). This is what guarantees
"render per surface" doesn't drift the surfaces apart.

Other risks to hold:

- **Replay re-projection** (SRD-76 re-runs the binder) must survive
  the typed-ring migration — replay re-projects typed events ×
  metrics-store query, not a stored render string.
- **`LogEntry` → typed spine record migration** touches the actor,
  observer, both sinks, and the TUI panel; stage behind push 1.
- **Don't event-source the fold projections.** `Update`/metric ticks
  stay re-derived from the snapshot, never appended to the stream
  (guardrail §3).
- **The ring is a render cache, not the spine.** Phase history is
  complete in the fold + session store; only *diagnostic* back-scroll
  is bounded by `LOG_RING_CAPACITY` on a long swap (covered by the
  existing "dropped N lines; see session.log" warning).
- **Ordering across threads.** Events append through the actor
  (typed command up), project through the ArcSwap snapshot ring
  (down) — same decoupling as SRD-02; no shared locks on the render
  path.

## 11. Open decisions

1. **Event enum unification** — ✅ RESOLVED (§4): the **kind-tag** is
   unified (`lifecycle::EventType`, now shared by the readout binder
   and `CheckpointData::event_type()`); the **data records stay
   separate per concern** (display spine vs. `CheckpointData`
   recovery log). Physical merge of the two records re-opens only in
   push 3 if a concrete need appears.
2. **TUI phase-metrics home** — ✅ RESOLVED (§5/§6): the existing
   tree-expanded detail block + active-phase panel (already projecting
   `phase_outcome`/`phase_status` via `TuiReadoutSink`); the log panel
   becomes diagnostics-only. No new panel; on-demand + Focus is enough.
3. **`session.log` outcome record** — ✅ RESOLVED (§6): option (A) —
   the complete plain-text projection (diagnostics + rich per-phase
   outcome rendered the original way, metric values from the store,
   no ANSI). Structured form additionally in sqlite + metrics store.
4. **Ring vs. canonical record** — ✅ RESOLVED (§3): the in-memory
   events ring (`LOG_RING_CAPACITY`) is a *render cache only*. The
   canonical durable record is the **session store** (sqlite
   `phase_outcomes` + metrics + checkpoint JSONL), written per phase
   on completion (`executor.rs`), plus the in-memory fold. No new
   event log. Replay re-projects from the session store; only
   diagnostic back-scroll is ring-bounded on a long swap.
