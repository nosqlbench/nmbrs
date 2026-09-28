// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Composable op dispenser wrappers.
//!
//! Each wrapper lives in its own submodule under this directory, and the
//! **module file is named for the YAML field that triggers it** — `poll.rs`
//! for `poll:`, `rate.rs` for `rate:`, and so on. The mod.rs holds shared
//! traits/types plus the wrappers still awaiting extraction; submodules are
//! re-exported here so the existing `crate::wrappers::<Name>` import paths
//! keep working.
//!
//! A wrapper declares itself to the registry with a [`crate::wrapper_registry::WrapperName`] and the
//! `owned_fields` that select it (see `crate::wrapper_registry`). At op
//! construction the resolver composes the ones whose fields are present into a
//! cascade around the adapter's dispenser, innermost-first, subject to each
//! wrapper's `requires_inner` / `forbids_outer` constraints.
//!
//! # The wrappers
//!
//! Ordered innermost-to-outermost, which is also the order a cycle passes
//! through them on its way to the adapter and back:
//!
//! | YAML | module | what it does |
//! |------|--------|--------------|
//! | `tries:` | [`tries`] | Attempt loop. The absolute innermost layer: wraps the raw adapter dispenser, owns the retry budget and the per-attempt panic catch. Absent `tries:`, it is not constructed at all and the op runs once. |
//! | `traverse:` | [`traverse`] | Result traversal. Counts result elements and bytes, walks declared capture points, and writes extracted values onto the per-fiber op-template kernel. On by default and installed even when nothing requires it; `result:` / `metrics:` additionally declare it `requires_inner`, which the composition resolver enforces at init. Like `result:` the block CUSTOMISES rather than selects: `path:` re-roots the body with a base JSON Pointer so captures stop repeating an envelope prefix, and `on_missing: ignore\|warn\|error` decides what an unresolved capture means. |
//! | `delay:` | [`delay`] | Sleeps before and/or after the inner op, reading the interval per cycle through the pull plan. `u64` is nanoseconds, `f64` is milliseconds. |
//! | `poll:` | [`poll`] | Re-executes the inner op until its ROW COUNT — optionally projected through a JSON Pointer — lands in `[min_rows, max_rows]`, or the timeout fires. The await primitive for backend state such as a compaction drain. Note the asymmetry with PHASE-level `poll:` ([`nmbrs_workload::model::PhasePollSpec`]), whose `until:` is a polydat boolean expression re-evaluated per iteration; the op-level wrapper has no such condition form and can only test a row count. |
//! | `if:` | `r#if` | Conditional execution. A falsy per-cycle value skips the op entirely — no inner execution, no adapter call — and counts a skip. |
//! | `while:` | `r#while` | Loops the inner op while a per-cycle predicate holds. |
//! | `result:` | [`result`] | Exposes op-result fields, plus the magic externs `body` / `count` / `ok`, as Polydat wires on the op-template kernel. Always installed; the optional `result:` block OVERRIDES which fields are exposed rather than enabling the wrapper — which is why it is not in the registry's `owned_fields`. |
//! | `metrics:` | [`metrics`] | Records synthetic metrics. After the adapter returns, pulls each declared metric's value through the op-template kernel and writes it to the instrument. Also resolves `cell:` placement, materialising one dimensional cell per coordinate value. |
//! | `rate:` | [`rate`] | Per-op rate limiter, independent of the activity-level limiter and of every other op's. Each instance owns its own limiter and acquires on every dispatch. |
//! | `fields:` | [`fields`] | Prints the rendered op text per cycle — the "what did this op actually send" surface. |
//! | `readout:` | [`readout`] | Opt-in per-op status visibility: reports an op-level lifecycle (start / complete / fail) so a long op appears as its own timed leaf rather than silence. |
//! | `memo:` | [`memo`] | Publishes a short human-visible string to the activity's memo slot before and/or after the op — the `[[ … ]]` line. |
//! | `gutter:` | [`gutter`] | Publishes the phase's contextual left-gutter cell from polydat templates. Distinct from `memo`, which owns the memo line. |
//! | `errors:` | [`errors`] | Error routing. The outermost OP-level wrapper: it sees the one terminal outcome of the whole stack and applies the op's error policy (warn / count / stop). |
//! | `dryrun:` | [`dryrun`] | Short-circuit: returns an empty result without calling the adapter. Its field is spelled like the others, but is normally INJECTED by the runner onto every op template from the CLI `dryrun=<mode>` param rather than written in a workload. It `forbids_outer` on `memo` and `gutter`, which is why those two hold explicit slots in the default order. |
//! | `interval:` / `repeat:` | [`interval`] | The one PHASE-level wrapper. Re-runs a whole phase, dwelling `interval` between runs and bounded by `repeat`. Not an `OpDispenser` — it wraps the phase seam, so there is no dispenser type to re-export.
//!
//! Composition order is not alphabetical or declaration order: it comes from
//! `wrapper_resolver::DEFAULT_ORDER` plus the constraint graph, and a workload
//! may override it with `wrappers:` / `--wrap-default-order`. Two placements
//! are load-bearing rather than cosmetic — `tries` is hand-placed innermost so
//! the plan matches runtime truth, and `memo`/`gutter` must sort inside
//! `dryrun` or resolution fails with `ForbiddenOuter`.

// Per-wrapper modules. As each wrapper migrates out of this
// file into its own module, add a `pub mod` line + re-export.
// SRD-82/92 — the first PHASE-level wrapper. Unlike the rest it is not an
// `OpDispenser`: a phase layer wraps the phase seam (`PhaseShell::run`), so
// there is no dispenser type to re-export.
// The one predicate mechanism, shared by `if:` / `while:` / `poll:`.
pub mod condition;
pub mod dryrun;
pub mod interval;
pub use dryrun::DryRunWrapper;
pub mod memo;
pub use memo::MemoDispenser;
pub mod gutter;
pub use gutter::GutterDispenser;
pub mod delay;
pub use delay::DelayDispenser;
pub mod r#if;
pub use r#if::ConditionalDispenser;
pub mod r#while;
pub use r#while::WhileWrapper;
pub mod rate;
pub use rate::OpRateWrapper;
pub mod fields;
pub use fields::FieldsDispenser;
pub mod poll;
pub use poll::{PollingDispenser, PollingMetrics};
pub mod readout;
pub use readout::ReadoutDispenser;
pub mod result;
pub use result::ResultDispenser;
pub mod metrics;
pub use metrics::MetricsDispenser;
pub mod traverse;
pub use traverse::{TraversalStats, TraversingDispenser};
pub mod tries;
pub use tries::TriesDispenser;
pub mod errors;
pub use errors::ErrorHandlerDispenser;

// All wrappers now live in their own submodules:
//   if / throttle.rs / poll / result.rs /
//   metrics.rs / fields.rs / memo.rs / dryrun / traverse.
