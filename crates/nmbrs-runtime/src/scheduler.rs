// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Scenario-tree scheduler: walks the [`ScopeTree`] and decides
//! when each scope's execution kicks off.
//!
//! The default [`SerialScheduler`] is a thin wrapper around
//! [`crate::executor::execute_tree`] — depth-first, one scope at
//! a time. The trait abstraction is the integration point for
//! future concurrent strategies; today only the serial behavior
//! is implemented.
//!
//! User config: `schedule=<level0>/<level1>/<level2>/...` per
//! SRD 18b §"Scheduler abstraction". A token of `1` is serial
//! at that level, `N>=2` allows up to N concurrent children, `*`
//! is unlimited; trailing levels inherit the previous setting.
//! Today the parser is functional but the only level it actually
//! enforces is "1" (serial); a non-trivial spec is recorded and
//! warned about, then the run proceeds serially.

use crate::executor::ExecCtx;
use nmbrs_workload::model::ScenarioNode;

// ─── ScheduleSpec ─────────────────────────────────────────────

/// Per-level concurrency limit declared by the user via
/// `schedule=<level0>/<level1>/...`. Each level corresponds to a
/// depth in the scope tree under the workload root. Trailing
/// levels inherit the last explicitly-declared value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleSpec {
    /// Per-level limits, in order from level 0 outwards.
    /// Implicitly extended by the last entry — `levels=[1, 4]`
    /// means level 0 is serial, levels 1+ are 4-way concurrent.
    pub levels: Vec<ConcurrencyLimit>,
}

/// Per-depth concurrency cap. Per SRD 02 §"One Concurrency Path":
/// concurrency is configuration, not a code branch. The walker /
/// dispatcher always runs through the semaphore-gated harness;
/// `Bounded(1)` produces sequential ordering naturally (the
/// semaphore admits one permit at a time, the JoinSet drains in
/// completion order = spawn order when only one is in-flight).
/// There is no separate "serial" code path anywhere; `Bounded(1)`
/// IS the serial case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyLimit {
    /// Up to N children concurrent. `Bounded(1)` is sequential.
    Bounded(u32),
    /// Unlimited concurrency.
    Unlimited,
}

impl ScheduleSpec {
    /// Default spec: sequential at every level (`Bounded(1)`).
    /// Equivalent to the pre-scheduler runner behavior; what
    /// callers get when no `schedule=` parameter is supplied.
    pub fn default_serial() -> Self {
        Self {
            levels: vec![ConcurrencyLimit::Bounded(1)],
        }
    }

    /// Parse the slash-separated form, e.g. `"1/4/*"`. Empty
    /// string and missing `schedule=` param both produce the
    /// default serial spec via the caller's `unwrap_or` flow.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("schedule spec is empty".into());
        }
        let mut levels = Vec::new();
        for (i, part) in s.split('/').enumerate() {
            let part = part.trim();
            let limit = match part {
                "*" => ConcurrencyLimit::Unlimited,
                _ => {
                    let n: u32 = part.parse().map_err(|_| {
                        format!("schedule spec level {i}: '{part}' is not a number or '*'")
                    })?;
                    if n == 0 {
                        return Err(format!(
                            "schedule spec level {i}: 0 is not a valid concurrency limit (use '1' for serial)"
                        ));
                    }
                    ConcurrencyLimit::Bounded(n)
                }
            };
            levels.push(limit);
        }
        Ok(Self { levels })
    }

    /// The effective concurrency limit at the given depth.
    /// Trailing depths beyond the spec inherit the last entry.
    /// A spec with no entries (shouldn't happen via `parse`) is
    /// treated as sequential.
    pub fn limit_at(&self, depth: usize) -> ConcurrencyLimit {
        if self.levels.is_empty() {
            return ConcurrencyLimit::Bounded(1);
        }
        let idx = depth.min(self.levels.len() - 1);
        self.levels[idx]
    }

    /// True when every level is sequential (`Bounded(1)`) — the
    /// spec is behaviorally equivalent to the default.
    #[allow(dead_code)] // used by unit tests; kept as spec-introspection API
    pub fn is_serial(&self) -> bool {
        self.levels
            .iter()
            .all(|l| matches!(l, ConcurrencyLimit::Bounded(1)))
    }
}

// ─── PhaseScheduler trait + impls ─────────────────────────────

/// Drives execution of a scenario tree under a concurrency
/// policy. Implementations decide how to interleave sibling
/// scopes at each depth, while the per-scope work itself
/// (compile-or-rebind, op execution, etc.) is the executor's
/// concern.
///
/// Today's only impl is [`SerialScheduler`]; the trait exists
/// so future concurrent strategies (level-bounded, work-stealing)
/// can plug in without touching the runner.
///
/// The `run` method returns a boxed future to keep the trait
/// dyn-compatible — `async fn` in trait position is currently
/// object-unsafe in stable Rust, and the existing `execute_tree`
/// already follows this convention.
pub trait PhaseScheduler {
    /// Run the scenario tree under this scheduler's policy.
    /// Mirrors the signature of [`crate::executor::execute_tree`]
    /// so callers can swap implementations at the runner level.
    /// Iter-var values flow through `ExecCtx::current_parent_kernel`
    /// per the M3.4b unified-comprehension contract — no
    /// separate HashMap parameter.
    fn run<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
        nodes: &'a [ScenarioNode],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Scenario-tree scheduler driven by [`ExecCtx::schedule_spec`].
///
/// A single impl handles every spec — the serial-everywhere case
/// (default) and any mix of bounded / unlimited levels. The
/// per-depth fork decision lives in [`crate::executor::execute_tree`];
/// this struct is the dyn-safe entry point callers use.
#[derive(Debug, Default)]
pub struct TreeScheduler;

impl PhaseScheduler for TreeScheduler {
    fn run<'a>(
        &'a self,
        ctx: &'a mut ExecCtx,
        nodes: &'a [ScenarioNode],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        crate::executor::execute_tree(ctx, nodes)
    }
}

/// Build the scheduler for a parsed `ScheduleSpec`. The returned
/// scheduler honors the spec's per-level policy end-to-end —
/// `Serial` levels walk siblings sequentially, `Bounded(N)` /
/// `Unlimited` levels fork per-sibling (and per-ForEach-iteration
/// at the corresponding depth) via cloned `ExecCtx` tasks on the
/// tokio runtime.
pub fn build(_spec: &ScheduleSpec) -> Box<dyn PhaseScheduler> {
    Box::new(TreeScheduler)
}

#[cfg(test)]
fn format_spec(spec: &ScheduleSpec) -> String {
    let parts: Vec<String> = spec
        .levels
        .iter()
        .map(|l| match l {
            ConcurrencyLimit::Bounded(n) => n.to_string(),
            ConcurrencyLimit::Unlimited => "*".into(),
        })
        .collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_serial() {
        // `1` is `Bounded(1)` — sequential is just N=1 through the
        // same harness; no separate variant.
        let s = ScheduleSpec::parse("1").unwrap();
        assert_eq!(s.levels, vec![ConcurrencyLimit::Bounded(1)]);
        assert!(s.is_serial());
    }

    #[test]
    fn parse_unlimited() {
        let s = ScheduleSpec::parse("*").unwrap();
        assert_eq!(s.levels, vec![ConcurrencyLimit::Unlimited]);
        assert!(!s.is_serial());
    }

    #[test]
    fn parse_multilevel() {
        let s = ScheduleSpec::parse("1/4/*").unwrap();
        assert_eq!(
            s.levels,
            vec![
                ConcurrencyLimit::Bounded(1),
                ConcurrencyLimit::Bounded(4),
                ConcurrencyLimit::Unlimited,
            ]
        );
    }

    #[test]
    fn limit_at_extends_trailing() {
        let s = ScheduleSpec::parse("1/4").unwrap();
        assert_eq!(s.limit_at(0), ConcurrencyLimit::Bounded(1));
        assert_eq!(s.limit_at(1), ConcurrencyLimit::Bounded(4));
        // Beyond explicit levels: inherit the last.
        assert_eq!(s.limit_at(2), ConcurrencyLimit::Bounded(4));
        assert_eq!(s.limit_at(99), ConcurrencyLimit::Bounded(4));
    }

    #[test]
    fn rejects_zero() {
        let err = ScheduleSpec::parse("0").unwrap_err();
        assert!(
            err.contains("0 is not a valid concurrency limit"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn rejects_garbage() {
        let err = ScheduleSpec::parse("foo").unwrap_err();
        assert!(err.contains("not a number"), "unexpected: {err}");
    }

    #[test]
    fn rejects_empty() {
        let err = ScheduleSpec::parse("").unwrap_err();
        assert!(err.contains("empty"), "unexpected: {err}");
    }

    #[test]
    fn n_one_is_serial() {
        // `1/1` is two levels of sequential (Bounded(1) at each).
        // is_serial() matches because every level is Bounded(1).
        let s = ScheduleSpec::parse("1/1").unwrap();
        assert!(s.is_serial());
    }

    #[test]
    fn default_serial_matches_explicit_one() {
        let default = ScheduleSpec::default_serial();
        let parsed = ScheduleSpec::parse("1").unwrap();
        assert_eq!(default, parsed);
    }

    #[test]
    fn build_returns_tree_scheduler_for_any_spec() {
        // Both serial and non-serial specs dispatch the single
        // TreeScheduler impl; concurrency is a per-depth decision
        // made inside the tree walk based on ExecCtx.schedule_spec.
        let _ = build(&ScheduleSpec::default_serial());
        let _ = build(&ScheduleSpec::parse("1/4/*").unwrap());
    }

    #[test]
    fn format_spec_round_trip() {
        let s = ScheduleSpec::parse("1/4/*").unwrap();
        assert_eq!(format_spec(&s), "1/4/*");
    }
}
