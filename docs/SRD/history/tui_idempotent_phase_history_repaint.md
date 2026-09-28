# TUI: Idempotent Phase-History Repaint on Terminal-State Transitions

> **Status (2026-06-07): SUPERSEDED — managed region REMOVED.**
> The managed phase-history region (a stateful screen-buffer for
> *unseen* history) was the wrong strategy and has been removed.
> See "Superseded (2026-06-07)" immediately below. The original
> SHIPPED notes are retained beneath it for provenance, but the
> region, its `phase_history.rs` projection, the `MAX_PHASE_HISTORY_
> ROWS`/`LOG_MIN_RESERVE` bounds, `bound_phase_history`, and the
> region's harness tests no longer exist.

## Superseded (2026-06-07) — region removed, history in scrollback

**Why.** Two problems converged. (1) The region duplicated the
**rich per-phase `✓` DONE summary** — the activity layer emits it
to scrollback via `diag!` (a `Diagnostic` log: `activity.rs`
`on_phase_end` render), which is richer than the region's terse
`✓ [N] name — Xs` line and carries the actual benchmark results
(label, recall, latencies). The region showed the same phases in a
worse format; the duplication surfaced as a burst after a console
log-flush. (2) Architecturally, **a screen-region buffer is not a
reliable way to hold unseen state** — it has to be reconstructed,
bounded, and kept idempotent across every geometry/mode change.

**The principle adopted.** *Don't keep unseen state in a screen
buffer.* History lives in the **data** (the log stream / the
`RunState` snapshot / `session.log`); the **active** display (the
status block) is re-derived from the snapshot on every sink
(re)start, so it redraws correctly on any mode activation
regardless of geometry. A phase appears on screen the same way for
a given run + terminal mode, independent of how many times modes
were swapped.

**What changed in code:**

- **Region deleted** from `log_only_sink.rs` — the phase-region
  computation, `redraw_bottom_region`'s `phase_lines`, the gutter,
  scroll-on-growth-for-region, the `phase_history_*` trackers, the
  `MAX_PHASE_HISTORY_ROWS`/`LOG_MIN_RESERVE` consts, and
  `bound_phase_history`. `crates/nmbrs-tui/src/phase_history.rs` is removed
  (module decl gone from `lib.rs`). The bottom region is now just
  `[status][inline-prompt]`.
- **History = scrollback.** The rich `✓` DONE lines already flow to
  scrollback (`activity.rs` `diag!`). The `LogCategory::Phase
  Lifecycle` skip now suppresses only the phase-**start** lifecycle
  noise (the live status block shows the running phase; the `✓`
  line is the completion marker). `session.log` keeps everything.
- **Swap-reliable scrollback** (`log_only_sink.rs` +
  `sink_supervisor.rs`): a cross-swap `Arc<AtomicU64>` `resume_from`
  cursor. Each `LogOnlySink` writes its final `last_seen` on
  shutdown and the next terminal-mode sink the supervisor brings up
  after a TUI swap **seeds `last_seen` from it** (`seed_last_seen`)
  instead of from the live `log_seq_total`. Combined with the
  alternate-screen save/restore (TUI and console both use it), the
  lines that scrolled while the alternate screen was up are
  re-emitted into the restored scrollback — so terminal scrollback
  = `f(log stream)`, reliable across Ctrl-T and console swaps.
  `RESUME_FRESH` (= `u64::MAX`) is the "first sink, start fresh"
  sentinel.
- **Tests:** `seed_last_seen_*` unit tests pin the seeding choice;
  the shadow-terminal harness (`tui_display_harness.rs`) was slimmed
  to the console-containment / byte-exact-surface-restore behavior
  it can still verify (anchored on plain `log` lines, since the
  mock actor doesn't run the readout engine that emits the rich
  phase lines); `ctrl_t_toggles_into_tui_and_back` re-anchored on
  the live status block's `ok:` field (the region glyph `▶` is gone
  from terminal mode).
- **Possible follow-up:** a deterministic E2E that asserts a log
  line emitted *during* a TUI session reappears in scrollback after
  Ctrl-T back (the resume path). Hard to time with a real workload;
  the unit test covers the seeding logic meanwhile.

---

> **Status (2026-06-06):** SHIPPED end-to-end. Tasks #29–#32 all
> landed: managed phase-history region with scroll-on-growth,
> phase-lifecycle scrollback suppression, idempotent repaint on
> Ctrl-T re-entry / console open+close, and console-output
> containment in the frame. Validated by pure unit tests plus a
> fully-encapsulated shadow-terminal harness. See "Shipped
> (implementation)" below; the original task notes are retained
> beneath it for provenance.
>
> **NOTE (2026-06-07):** the region described in this SHIPPED
> section was subsequently removed — see "Superseded" above.

## Shipped (implementation — 2026-06-06)

Production (all in `nmbrs-tui`, no test-only escape hatches):

- **Managed phase-history region** (`log_only_sink.rs`
  `run_render_loop`): a region painted above the status block from
  `phase_history::phase_history_lines(&snap)`, bounded by
  `MAX_PHASE_HISTORY_ROWS` (12) and `LOG_MIN_RESERVE` (4) — tail +
  `⋯ N earlier` marker on overflow. Marked dirty by comparing the
  unbounded projection against the last committed one.
- **One ordered redraw pass** replaced the old clear→emit→redraw
  trio: compute content+geometry → **scroll-on-growth** (emit
  newlines at the bottom row so a newly-started phase scrolls a
  log line into scrollback instead of clobbering it) →
  `clear_bottom_rows` → log emit → `redraw_bottom_region`
  (extended to take `phase_lines: &[String]`). Steady-state ticks
  with no change still touch nothing (no prompt drift).
- **Phase-lifecycle scrollback suppression**: a `LogCategory`
  (`Diagnostic` | `PhaseLifecycle`) threads diag → `observer::log_
  categorized` → `RunObserver::log_categorized` (default delegates)
  → `RunStateCmd::Log` → `LogEntry.category`. `fire_lifecycle`
  tags `PhaseStart`/`PhaseEnd` renders `PhaseLifecycle`; the sink
  skips those in its scrollback drain (they live in the region).
  `session.log` keeps everything. Scope/iteration/session slots
  stay `Diagnostic` — scope nodes are stored-`Pending` so they
  never appear in the region.
- **Transition repaint** falls out of the managed region: a fresh
  sink on Ctrl-T re-entry repaints the region from the snapshot on
  its first redraw (catch-up); `repl_changed` repaints on console
  open/close. Window mode suppresses the region; closing repaints.
- **Console-output containment** (#31): REPL command echo /
  response / completions go to the transcript ring
  (`repl_state::push_transcript` / `push_transcript_line`), never
  stderr scrollback. Any multi-row console frame (Window, or a Bar
  grown past one row) renders the transcript tail in its bounded
  region with internal scrollback. A single-row Bar shows only the
  input; its output stays in the ring (view by growing the bar or
  opening the window) and never scrolls the terminal.

### How it's tested (no production changes to enable testing)

- Pure renderer: byte-level unit tests in `log_only_sink.rs`
  (`redraw_tests`) pin the absolute positions of the
  phase/status/prompt rows; `phase_history.rs` tests pin the
  projection.
- **Fully-encapsulated E2E**: `crates/nmbrs/examples/tui_display_harness.rs`
  drives the real `LogOnlySink` from a mock run-state actor
  (`spawn_run_state_actor` + `RunStateCmd`) under
  `shadow_terminal`, lock-stepped via stdin commands
  (`tree`/`start`/`done`/`bar`/`window`/`out`). Tests in
  `crates/nmbrs/tests/tui_display_harness.rs`: region renders started
  phases (in-place glyph flip, no dup), bar-toggle idempotency,
  window-close idempotency, console-output containment. The
  harness reads stdin in no-echo raw mode (termios) so command
  bytes don't paint the surface. It uses only `nmbrs-tui`'s public
  API.
- **Real-run Ctrl-T swap** (`crates/nmbrs/tests/tui_terminal_toggle.rs`
  `ctrl_t_toggles_into_tui_and_back`, formerly `#[ignore]`d): the
  harness can't exercise the supervisor + TuiSink swap, so this
  spawns the real `nmbrs` under `shadow_terminal` and toggles
  terminal → TUI → terminal. The old MetricsQuery race is gated
  out by construction — it waits for the region's `▶` before
  Ctrl-T, and a phase only reaches `Running` after the runner's
  `observer.on_metrics_query(...)` in execution setup, so the
  query is wired by then. The swap-back `▶` also re-verifies the
  catch-up repaint in a real run. Driven with a paced op
  (`rate: "5/s"`) + `filename=/dev/null` so the stdout adapter
  emits nothing to fight the region; no production change.

## Follow-up (2026-06-07): console on the alternate screen

Live use surfaced two issues with the inline console: (1) toggling
it with `~` left residual state (a blank gap), and (2) closing it
visibly re-streamed the reconstruction. Root cause: the console
rendered **inline** on the primary screen, so opening it scrolled
the logs up (grow-on-scroll) and closing couldn't un-scroll —
leaving a gap. The Ctrl-T TUI never had this because it uses the
**alternate screen**: the terminal saves the primary surface on
enter and restores it byte-exact on leave (the built-in "backing
buffer").

Fix (chosen via AskUserQuestion — "alt-screen console (Bar+Window)"):
the console now lives on the alternate screen too. In
`log_only_sink.rs::run_render_loop`:
- When the REPL becomes visible (Bar or Window), write
  `\x1b[?1049h` (terminal saves the primary), save the primary-
  region trackers, and render the console full-screen on the alt
  surface via `redraw_console_altscreen` (header + transcript tail
  + input). The log drain is **frozen** (`last_seen` held) so
  buffered logs flush after leaving.
- When it goes Hidden, write `\x1b[?1049l` (terminal restores the
  primary byte-exact), restore the trackers, and sync
  `repl_visibility_drawn` to Hidden so an unchanged surface is left
  untouched (no re-stream). Real catch-up (buffered logs, phase
  progress, the timer) flows through the normal dirty signals.
- The console repaints on any transcript change via
  `repl_state::transcript_len()` (not just dispatched commands),
  so output from any source shows.

### Phase-region gutter alignment (2026-06-07)

Live use also showed the managed phase-history region drawn at
column 1 (only its depth indent), while every log + status line
carries the `Xs N/total │` margin — so the phase tree floated out
of alignment ("busted layout"). Fix: `redraw_bottom_region` now
prefixes each phase row with a **blank gutter** the same visible
width as the margin, ending in the `│` divider (dim), so the
phase tree lines up under the same content column as the logs and
status. The gutter is blank (no timer) so it doesn't repeat the
clock on every row and is byte-stable across redraws (the
byte-exact restore test still holds). `margin_width == 0` (the
pure-layout unit tests / piped runs) yields no gutter.

A second pass made the region **flat**: `phase_history_lines` no
longer indents by `"  ".repeat(depth)`. The region shows only
phases (scope nodes are stored-`Pending` and filtered out), so a
depth indent tracked *invisible* scope ancestors and made the
phase sequence jump horizontally (a top-level reset at indent 2
next to a deeply-nested phase at indent 16). The `[seq]` prefix
conveys order; the rows now all start at the same column after the
gutter. Result:

```
0.15091s   2/2 │ scrollback-line-1
0.15091s   2/2 │ scrollback-line-2
               │ ✓ [1] schema — 1.23s
               │ ▶ [2] main
```

Open follow-up (not done): the running phase appears in both the
region (`▶ …`) and the live status block — mild redundancy. Could
filter Running out of the region (history = completed/failed only;
status = current), but `phase_history_lines` includes Running by
design (SRD decision 3) and the harness asserts it, so left as-is.

Net: opening / closing the console is a clean save/restore — zero
residual, no re-stream — and console output is fully contained
(it's on the alt surface and never touches the primary). The
inline Bar-vs-Window distinction is now vestigial (both are the
alt-screen console); the old inline window-mode composition in the
primary path is dead (reached only when Hidden) and can be removed
in a later cleanup. Validated end-to-end:
`console_toggle_restores_surface_byte_exact` (S0 == surface after
close, byte-exact) and `console_output_is_contained_in_frame`
(output shows in the console, never leaks to the primary).

---

## Original task notes (provenance)

> **Status (2026-06-06):** DESIGN APPROVED + foundational primitive
> shipped and tested. Render-loop / REPL-window wiring remains
> (tasks below). This doc is the cross-session handoff.

## Goal (two user requirements, one mechanism)

1. **Ctrl-T terminal-format change** must *re-layout from the phase
   history idempotently each time*, preserving the look of each
   terminal mode (so toggling never loses or duplicates phases —
   the terminal "catches up" as if it had never been disrupted).
2. **Shell prompt in the TUI console** must not scroll the whole
   terminal state — its output stays *contained within the console
   frame*. Closing the frame with `~` redraws the terminal state
   idempotently from phase history, exactly like a mode change.

Both collapse to one invariant:

> **terminal state = f(phase history)** — a single idempotent
> repaint primitive, invoked at every display-state transition
> (Ctrl-T toggle, console open/close). No transition may leave
> accumulated or missing state.

## Approved design decisions

- **(1a) Managed region, not full clear.** The phase history paints
  into a *managed region* above the status region — the painter
  clears+repaints just that region (mirroring the existing status-
  region discipline), preserving the log scrollback above it.
  (Rejected: full-screen clear, which would discard scrollback.)
- **(2) Bounded in-frame console buffer.** The console (REPL
  Bar/Window) output renders into the frame's own fixed-height
  region with internal scrollback — older output scrolls *within*
  the frame, never the terminal.
- **(3) Catch-up = started phases only.** The layout reconstructs
  the *active + completed* phase history (Running / Completed /
  Failed) in scene-tree DFS order — "catch up to current state as
  if it weren't disrupted." `Pending` phases are omitted (an
  uninterrupted terminal wouldn't have printed them yet). NOT the
  full tree projection.

## Shipped (foundational contract — DONE, tested)

`crates/nmbrs-tui/src/phase_history.rs` (registered in `lib.rs`):

- `phase_history_lines(snap: &RunState) -> Vec<String>` — pure
  projection of `snap.phases` (the denormalized DFS `PhaseEntry`
  view). One line per started phase: depth indent + status glyph
  (`▶`/`✓`/`✗`, matching the TUI tree) + `[seq]` + name +
  duration (Completed) / error (Failed). **Idempotent by
  construction**: identical snapshot ⇒ byte-identical output.
- 3 unit tests: idempotency (N calls identical), pending-omission,
  per-status formatting. `cargo test -p nmbrs-tui --lib phase_history`.

This is the function both transitions call. nmbrs-tui: 73/0, 0 warnings.

## Remaining work (tasks #29–#32)

### #29 — Wire painter into LogOnlySink managed region
- `crates/nmbrs-tui/src/log_only_sink.rs` `run_render_loop`: add a managed
  **phase-history region** above the existing status region. Follow
  the existing clear+repaint discipline (`clear_combined_region`,
  absolute positioning — see the bottom-region block ~L334–L595).
  Paint `phase_history::phase_history_lines(&snap)`; mark dirty when
  the phase set changes (a cheap len/last-status check, or hash).
- **Suppress `phase_outcome` from the log scrollback in terminal
  mode** so completed phases don't appear twice (once in the
  managed region, once as a scrollback log line). Check how the
  `phase_outcome` readout reaches `log_messages` and gate it.

### #30 — Repaint on Ctrl-T re-entry + `~`-close
- Ctrl-T re-entry: `crates/nmbrs-tui/src/sink_supervisor.rs` restarts the
  `LogOnlySink` and currently **skips replay** (bumps
  `last_seen_seq`; comment "no replay of historical buffer" ~L207).
  Replace with: on terminal-mode entry, paint the phase-history
  region from the current snapshot (catch-up; idempotent).
- `~`-close: when the console frame transitions to Hidden, repaint
  the phase-history region (same painter).

### #31 — Console-frame output containment
- REPL command output currently prints to stderr scrollback
  (`log_only_sink.rs` ~L306–L329) AND pushes to the transcript ring
  (`repl_state::push_transcript`) for Window mode. Make Bar/Window
  output render into the frame's **bounded in-frame buffer** (fixed
  region, internal scrollback) instead of the shared append stream,
  so it can't scroll the terminal state.

### #32 — Shadow-terminal idempotency E2E test
- Extend `crates/nmbrs/tests/tui_terminal_toggle.rs` (uses
  `shadow_terminal::SteppableTerminal` — an in-memory rendered
  terminal driven via PTY). Assert: (a) Ctrl-T N times → identical
  rendered cells on each terminal-mode entry; (b) open console,
  emit output, `~`-close → terminal cells == pre-open phase-history
  layout. This is the only surface that exercises the cursor/region
  accounting end-to-end.

## Key facts / code map

| Thing | Location |
|---|---|
| Ctrl-T → toggle | `key_watcher.rs:204` (`ToggleTui`); `app.rs:952` (`yielded_to_terminal`) |
| `~` → REPL cycle toward Hidden | `key_watcher.rs:244` (`ReplToggleBar`); backtick → Window |
| Canonical phase history | `state.rs` `RunState { tree, phases: Vec<PhaseEntry>, active_phases }` (actor + ArcSwap snapshot) |
| `PhaseEntry` / `PhaseStatus` | `state.rs:178`; `scene_tree.rs:121` (`Pending/Running/Completed/Failed(String)`) |
| TUI render (already idempotent) | `tui_sink.rs` — pure function of snapshot each frame |
| Terminal render loop (FRAGILE) | `log_only_sink.rs` `run_render_loop` — managed bottom region, absolute positioning |
| Toggle / sink lifecycle | `sink_supervisor.rs` |
| REPL state machine | `repl_state.rs` (Hidden/Bar/Window) |
| Shipped primitive | `phase_history.rs` |

## Fragility warning

The `log_only_sink.rs` render loop's cursor/region accounting is
**explicitly fragile** (its own comments: "critical fix to keep the
prompt from drifting down the screen as `\r\n` separators stack
up"). Changes there must be validated through the shadow-terminal
harness (#32), not by reasoning alone — there is no piped-output
surface that reveals cursor drift. Do #32 early as the safety net,
or alongside #29.
