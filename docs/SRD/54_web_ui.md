# 54: Web UI — Contract & Axioms (nmbrs-web)

Front door for **nmbrs-web** (layer L5): the Axum + Askama + htmx dashboard — live metrics,
dynamic controls, a Polydat function browser, and a DAG viewer. Pillars 1+2 of the
[Subsystem Treatment Standard](00b_subsystem_standard.md).

> The mechanism-level detail (route map, embedded-vs-standalone modes, the graph editor)
> currently lives in `docs/SRD/notes/32_web_ui.md` and folds into this doc's Pillar-3
> section in Part 4 of the consolidation.

## Contract

**Surface, inbound contract, and allowed edges:** authoritative in
[SRD 05 §Contract Registry](05_dependency_rules.md). In brief — exports `server` (router
assembly; embedded + standalone) + `ws` (WebSocket metric fanout); `routes`/`models`/`graph`
are internal; consumes `nmbrs_runtime` + `nmbrs_metrics` (the read-side metric/control surface).

## Axioms

- **WB1 — Server-rendered, no SPA.** Askama templates + htmx; no client-side JS framework.
- **WB2 — Read-side projection.** The dashboard projects `metrics_query` + `controls` —
  the same state source the TUI reads, not a parallel store. [SRD 81](81_event_sourced_display.md).
- **WB3 — Mutations via confirmed-apply controls only.** State changes go through the
  dynamic-control write path, never a direct poke. [SRD 23](23_dynamic_controls.md).

## Mechanism (Pillar 3)
Route/handler map + graph editor: `docs/SRD/notes/32_web_ui.md` (to fold here in Part 4).
Shared concepts: [SRD 23](23_dynamic_controls.md) (controls), [SRD 81](81_event_sourced_display.md)
(projection model).

## See also
`crates/nmbrs-web/src/lib.rs`; [SRD 23](23_dynamic_controls.md); [SRD 81](81_event_sourced_display.md).
