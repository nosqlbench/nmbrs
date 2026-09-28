# 59: Terminal UI — Contract & Axioms (nmbrs-tui)

Front door for **nmbrs-tui** (layer L5): the ratatui live dashboard and the `TuiObserver`
that drives it from the run's event stream. Pillars 1+2 of the
[Subsystem Treatment Standard](00b_subsystem_standard.md).

## Contract

**Surface, inbound contract, and allowed edges:** authoritative in
[SRD 05 §Contract Registry](05_dependency_rules.md). In brief — exports the `TuiObserver` +
`RunState` + the sink seam + the inspector REPL; consumes `nmbrs_runtime::observer` and the
`nmbrs_metrics` snapshot it folds; the render/dispatch internals (`widgets`, `frame_broker`,
`readout_panel`, `readout_sink`, `prompt_state`, `tui_sink`) are `pub(crate)` (SRD-05 D5).

## Axioms

- **T1 — No screen-buffer for unseen state.** History lives in the log stream /
  `session.log`; a managed screen region holds only the live **active** display and
  re-derives it from source on mode activation, regardless of geometry.
  [SRD 81](81_event_sourced_display.md).
- **T2 — Display = fold of a typed event stream** — the engine-wide axiom
  [SRD 29 A8](29_execution_engine.md); `RunState` is the TUI's fold, and no surface consumes a
  string rendered for another.
- **T3 — Actor + `ArcSwap`, never shared locks** — the [SRD 39 M1](39_metrics_contract.md) pattern
  applied to render state (no `std::RwLock`: Linux's writer-preferring `RwLock` deadlocks on
  nested reads).
- **T4 — The console belongs to the adapter** — see [SRD 29 A6](29_execution_engine.md) /
  [SRD 41 §Output Routing](41_logging.md).

## Mechanism (Pillar 3)
[SRD 62](62_tui_layout.md) (layout), [SRD 63](63_status_readouts.md) (status readouts),
[SRD 81](81_event_sourced_display.md) (event-sourced display projections).

## See also
`crates/nmbrs-tui/src/lib.rs`; [SRD 62](62_tui_layout.md); [SRD 63](63_status_readouts.md); [SRD 81](81_event_sourced_display.md).
