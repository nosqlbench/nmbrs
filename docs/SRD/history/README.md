# SRD History — archived design records

A small set of design notes whose substance is **not** well-represented anywhere else —
kept for the *why*, not because they're authoritative:

- **Algorithm / scenario sketches** with no dedicated design doc (only the code +
  catalog cover them): `09_alias_method`, `11_icd_sampling`, `25_pcg_rng`,
  `08_gk_performance_scenarios`.
- **One-time records**: `tui_idempotent_phase_history_repaint` (the rationale for an approach that was later
  **reversed** — see SRD 81), `nosqlbench_binding_patterns` (a compatibility scan),
  `resumable_test_fixture` (a test-fixture design memo).

Anything whose authoritative form lives in a live SRD, in `polydat/docs/design/`, or in
the code has been removed (recoverable via git) — it added duplication, not rationale.

The canonical reference is the numbered SRD set in the parent directory: start at
[../00_index.md](../00_index.md) or [../../SYSREF.md](../../SYSREF.md). Living (non-archived)
design rationale is in [../notes/](../notes).
