// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Phase `throttle:` — the adaptive backpressure governor (SRD-83
//! Part 9).
//!
//! A saturated target converts client overload into retry churn: the
//! tries wrapper absorbs server rejections, `ok:%` stays green, and
//! the only truthful signal is the ATTEMPT plane — the windowed
//! attempt-failure fraction (`attempt_failure / resolved attempts`,
//! the see-through-retries view).
//!
//! The governor assumes the MOST FRAGILE target by default and scales
//! to robust ones, TCP-style:
//!
//! - **Slow-start.** The offered value begins at `start` (default:
//!   `floor`) — the authored `concurrency:`/`rate:` is the CEILING,
//!   not the opening offer. While no congestion has been seen, each
//!   clean window DOUBLES the offer toward the ceiling: a robust
//!   target climbs to full load in a handful of windows with zero
//!   failures; a fragile one is never assaulted at all. A target
//!   known to be robust at phase entry declares `start:` explicitly.
//! - **Severity-proportional back-off.** Above `high`, the offer is
//!   multiplied by `clamp(1 − frac, 0.25, 0.9)`: a marginal breach
//!   trims gently (×0.9), total failure collapses fast (×0.25),
//!   never below `floor`.
//! - **Congestion memory.** Each back-off records the offer at which
//!   failure was observed (`last_bad`). Recovery climbs ×1.5 through
//!   the proven-safe zone (up to 75% of `last_bad`), then probes
//!   ADDITIVELY (+max(1, 2% of `last_bad`) per clean window) — no
//!   more marching multiplicatively back into the same wall.
//!   [`MEMORY_CLEAR_STREAK`] CONSECUTIVE clean windows at-or-above
//!   `last_bad` clear the memory (the target got healthier — warmed
//!   caches, finished compactions), restoring the multiplicative
//!   climb. One clean window is not evidence: the additive probes
//!   keep stepping through the streak, so each window in it sits a
//!   notch higher than the last, and a single quiet window at a
//!   marginal congestion point can never re-arm the doubling climb
//!   straight back into the wall.
//!
//! Windows are computed from counter DELTAS on the drain-loop tick —
//! a true trailing window, never a lifetime average. Writes ride the
//! push-on-set control path (`ControlOrigin::Governor`,
//! confirmed-apply, spawned off the loop); every movement logs one
//! line naming the signal — visible, never silent.
//!
//! Measurement honesty: the throttled steady state IS the
//! measurement — the target's capacity at the declared failure
//! bound. A load figure taken at high attempt-failure is a
//! saturation artifact.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nmbrs_metrics::controls::{ControlOrigin, ErasedControl};

/// All governor lines carry the `Throttle` category (in-flight —
/// governance happens mid-body, attached to no boundary), so sinks
/// and counters can dispatch on the axis instead of matching the
/// rendered `throttle:` prefix.
macro_rules! gov_log {
    ($level:expr, $($arg:tt)*) => {
        crate::observer::log_tagged(
            $level,
            crate::observer::EventTag::in_flight(
                crate::observer::EventCategory::Throttle),
            &format!($($arg)*),
        )
    };
}

/// What one window decided — pure, unit-testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// Signal above `high`: back off to the contained value.
    Down(f64),
    /// Clean window with headroom: raise to the contained value.
    Up(f64),
    /// Hold (dead band, no traffic, or at a bound).
    Hold,
}

/// Consecutive clean windows at-or-above the remembered congestion
/// point required before the memory clears and the multiplicative
/// climb resumes. Additive probing continues through the streak, so
/// clearing means the target stayed clean across a rising run of
/// offers, not one lucky window.
pub const MEMORY_CLEAR_STREAK: u32 = 3;

/// Pure congestion-memory step: fold one window's evidence (`clean
/// at-or-above last_bad`) into the running streak. Returns the new
/// streak and whether the memory clears on this window. Any window
/// without that evidence — a breach, a dead-band hold, or a clean
/// window still below the congestion point — resets the streak.
pub fn memory_clear_step(streak: u32, evidence: bool) -> (u32, bool) {
    if !evidence {
        return (0, false);
    }
    let streak = streak + 1;
    if streak >= MEMORY_CLEAR_STREAK {
        (0, true)
    } else {
        (streak, false)
    }
}

/// Pure governor step. `frac` is the windowed attempt-failure
/// fraction, `current` the committed offer, `last_bad` the offer at
/// which congestion was last observed (`None` = unexplored — slow
/// start).
pub fn decide(
    frac: f64,
    current: f64,
    high: f64,
    low: f64,
    floor: f64,
    ceiling: f64,
    last_bad: Option<f64>,
) -> Decision {
    if frac > high {
        // Severity-proportional multiplicative decrease: marginal
        // breach trims ×0.9; total failure collapses ×0.25.
        let target = (current * (1.0 - frac).clamp(0.25, 0.9)).max(floor);
        if target < current {
            return Decision::Down(target);
        }
        return Decision::Hold;
    }
    if frac < low && current < ceiling {
        let target = match last_bad {
            // Unexplored territory: slow-start doubling.
            None => (current * 2.0).max(current + 1.0),
            Some(bad) => {
                // Fast reclimb through the proven-safe zone, then
                // cautious additive probing toward the old wall.
                let safe = (bad * 0.75).max(floor);
                let fast = (current * 1.5).min(safe);
                if fast > current {
                    fast
                } else {
                    current + (bad * 0.02).max(1.0)
                }
            }
        }
        .min(ceiling);
        if target > current {
            return Decision::Up(target);
        }
    }
    Decision::Hold
}

/// The per-phase governor: window bookkeeping over the activity's
/// cumulative attempt counters plus the resolved control handle.
pub struct ThrottleGovernor {
    control: Arc<dyn ErasedControl>,
    control_name: String,
    phase_name: String,
    high: f64,
    low: f64,
    floor: f64,
    ceiling: f64,
    start: f64,
    window: Duration,
    window_start: Instant,
    base_success: u64,
    base_failure: u64,
    /// The offer at which congestion was last observed. `None` =
    /// unexplored (slow-start regime).
    last_bad: Option<f64>,
    /// Consecutive clean windows observed at-or-above `last_bad`;
    /// the memory clears at [`MEMORY_CLEAR_STREAK`].
    clean_streak: u32,
}

impl ThrottleGovernor {
    /// Resolve the governor from the phase's declared spec against the
    /// attached component (where `Activity::attach_component` declared
    /// the controls). Returns `None` — with a logged warning, never
    /// silently — when the named control is not declared (e.g.
    /// `control: rate` on a phase without `rate:`).
    ///
    /// Construction immediately publishes the slow-start offer to the
    /// control (push-on-set), so the phase OPENS at `start`, not at
    /// the authored ceiling; the caller also reads
    /// [`Self::initial_concurrency`] to spawn the fiber pool at the
    /// same offer.
    pub fn from_spec(
        spec: &nmbrs_workload::model::ThrottleSpec,
        component: Option<&Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>>,
        phase_name: &str,
        authored_concurrency: usize,
        authored_rate: Option<f64>,
    ) -> Option<Self> {
        let Some(component) = component else {
            gov_log!(
                crate::observer::LogLevel::Warn,
                "phase '{phase_name}': throttle: no component attached — \
                 governor disabled"
            );
            return None;
        };
        let control = component
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .find_control_erased_up(&spec.control);
        let Some(control) = control else {
            gov_log!(
                crate::observer::LogLevel::Warn,
                "phase '{phase_name}': throttle: control '{}' is not \
                 declared on this phase — governor disabled \
                 (`control: rate` needs a `rate:` on the phase)",
                spec.control
            );
            return None;
        };
        let ceiling = match spec.control.as_str() {
            "rate" => authored_rate.unwrap_or(f64::INFINITY),
            _ => authored_concurrency as f64,
        };
        let start = spec.start.unwrap_or(spec.floor).clamp(
            spec.floor,
            if ceiling.is_finite() {
                ceiling
            } else {
                f64::MAX
            },
        );
        let window = nmbrs_workload::magnitude::parse_magnitude(&spec.window)
            .map(Duration::from_secs_f64)
            .or_else(|| {
                crate::timeval::parse_time_ms(&spec.window)
                    .ok()
                    .map(Duration::from_millis)
            })
            .unwrap_or(Duration::from_secs(2));
        let low = spec.low.unwrap_or(spec.high / 5.0);
        gov_log!(
            crate::observer::LogLevel::Info,
            "phase '{phase_name}': throttle: governing '{}' — slow-start \
             at {} toward ceiling {} (floor {}); back off above {:.1}% \
             windowed attempt failure ({}), recover below {:.1}%",
            spec.control,
            fmt_val(start),
            fmt_val(ceiling),
            fmt_val(spec.floor),
            spec.high * 100.0,
            spec.window,
            low * 100.0
        );
        let governor = Self {
            control,
            control_name: spec.control.clone(),
            phase_name: phase_name.to_string(),
            high: spec.high,
            low,
            floor: spec.floor,
            ceiling,
            start,
            window,
            window_start: Instant::now(),
            base_success: 0,
            base_failure: 0,
            last_bad: None,
            clean_streak: 0,
        };
        // Publish the opening offer so the control's committed value
        // reflects the slow-start from the first cycle. (For a
        // concurrency governor the caller ALSO spawns the pool at
        // `initial_concurrency`, so the offer and the pool agree.)
        if start < ceiling {
            governor.write(start);
        }
        Some(governor)
    }

    /// The fiber count the activity should OPEN with when this
    /// governor walks `concurrency` — the slow-start offer, not the
    /// authored ceiling. `None` when the governor walks `rate` (the
    /// pool spawns at the authored concurrency; the rate limiter
    /// carries the slow-start instead).
    pub fn initial_concurrency(&self) -> Option<usize> {
        (self.control_name == "concurrency").then_some((self.start.max(1.0)) as usize)
    }

    /// Called every activity-loop pass with the CUMULATIVE attempt
    /// counters; acts at most once per window. The control write is
    /// push-on-set (spawned, confirmed-apply) — the loop never blocks.
    pub fn tick(&mut self, attempt_success: u64, attempt_failure: u64) {
        if self.window_start.elapsed() < self.window {
            return;
        }
        let d_success = attempt_success.saturating_sub(self.base_success);
        let d_failure = attempt_failure.saturating_sub(self.base_failure);
        self.window_start = Instant::now();
        self.base_success = attempt_success;
        self.base_failure = attempt_failure;

        let d_total = d_success + d_failure;
        if d_total == 0 {
            return;
        }
        let frac = d_failure as f64 / d_total as f64;
        // The committed value is the truth to step from — external
        // writers (TUI, web, control_set) are honored, not fought.
        let Some(current) = self.control.gauge_f64() else {
            return;
        };

        // A SUSTAINED run of clean windows at-or-above the remembered
        // congestion point means the target got healthier (caches
        // warm, compactions done): clear the memory and resume the
        // multiplicative climb. One clean window at a marginal
        // congestion point is noise, not evidence — the additive
        // probes keep stepping through the streak, so clearing means
        // the target stayed clean across a rising run of offers.
        if let Some(bad) = self.last_bad {
            let evidence = frac < self.low && current >= bad;
            let (streak, clears) = memory_clear_step(self.clean_streak, evidence);
            self.clean_streak = streak;
            if clears {
                gov_log!(
                    crate::observer::LogLevel::Info,
                    "throttle: phase '{}': {} consecutive clean windows at or \
                     above prior congestion point {} (now at {}) — memory \
                     cleared, resuming climb",
                    self.phase_name,
                    MEMORY_CLEAR_STREAK,
                    fmt_val(bad),
                    fmt_val(current)
                );
                self.last_bad = None;
            } else if evidence {
                gov_log!(
                    crate::observer::LogLevel::Debug,
                    "throttle: phase '{}': clean window at {} ≥ prior \
                     congestion point {} ({streak}/{} toward clearing memory)",
                    self.phase_name,
                    fmt_val(current),
                    fmt_val(bad),
                    MEMORY_CLEAR_STREAK
                );
            }
        }

        match decide(
            frac,
            current,
            self.high,
            self.low,
            self.floor,
            self.ceiling,
            self.last_bad,
        ) {
            Decision::Down(target) => {
                gov_log!(
                    crate::observer::LogLevel::Warn,
                    "throttle: phase '{}': windowed attempt failure {:.1}% \
                     ({d_failure}/{d_total} over {:.1}s) > {:.1}% — {} {} → {}",
                    self.phase_name,
                    frac * 100.0,
                    self.window.as_secs_f64(),
                    self.high * 100.0,
                    self.control_name,
                    fmt_val(current),
                    fmt_val(target)
                );
                self.last_bad = Some(current);
                self.write(target);
            }
            Decision::Up(target) => {
                let mode = match self.last_bad {
                    None => "climbing",
                    Some(bad) if target < bad * 0.75 => "reclimbing",
                    Some(_) => "probing",
                };
                gov_log!(
                    crate::observer::LogLevel::Info,
                    "throttle: phase '{}': windowed attempt failure {:.1}% \
                     < {:.1}% — {mode} {} {} → {} (ceiling {})",
                    self.phase_name,
                    frac * 100.0,
                    self.low * 100.0,
                    self.control_name,
                    fmt_val(current),
                    fmt_val(target),
                    fmt_val(self.ceiling)
                );
                self.write(target);
            }
            Decision::Hold => {}
        }
    }

    fn write(&self, target: f64) {
        let control = self.control.clone();
        let origin = ControlOrigin::Governor {
            source: format!("throttle:{}", self.phase_name),
        };
        let name = self.control_name.clone();
        let phase = self.phase_name.clone();
        tokio::spawn(async move {
            if let Err(e) = control.set_f64(target, origin).await {
                gov_log!(
                    crate::observer::LogLevel::Warn,
                    "throttle: phase '{phase}': write {name}={target} \
                     failed: {e}"
                );
            }
        });
    }
}

fn fmt_val(v: f64) -> String {
    if v.is_infinite() {
        "∞".to_string()
    } else if (v.fract()).abs() < 1e-9 {
        format!("{}", v as i64)
    } else {
        format!("{v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Severity-proportional back-off: total failure collapses fast,
    /// a marginal breach trims gently, the floor contains the walk.
    #[test]
    fn backoff_scales_with_severity() {
        // 100% failure → ×0.25.
        assert_eq!(
            decide(1.0, 100.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Down(25.0)
        );
        // Marginal breach (7%) → ×0.9 trim.
        assert_eq!(
            decide(0.07, 100.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Down(90.0)
        );
        // Mid-severity (50%) → ×0.5.
        assert_eq!(
            decide(0.5, 40.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Down(20.0)
        );
        // The floor contains the collapse; at the floor, hold.
        assert_eq!(
            decide(1.0, 5.0, 0.05, 0.01, 4.0, 100.0, None),
            Decision::Down(4.0)
        );
        assert_eq!(
            decide(1.0, 4.0, 0.05, 0.01, 4.0, 100.0, None),
            Decision::Hold
        );
    }

    /// Unexplored territory (slow-start): clean windows DOUBLE the
    /// offer toward the ceiling — a robust target reaches full load
    /// in log2(ceiling/start) windows with zero failures.
    #[test]
    fn slow_start_doubles_while_clean() {
        assert_eq!(
            decide(0.0, 1.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Up(2.0)
        );
        assert_eq!(
            decide(0.0, 8.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Up(16.0)
        );
        // Ceiling contains the climb; at the ceiling, hold.
        assert_eq!(
            decide(0.0, 64.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Up(100.0)
        );
        assert_eq!(
            decide(0.0, 100.0, 0.05, 0.01, 1.0, 100.0, None),
            Decision::Hold
        );
    }

    /// After congestion at `last_bad`, recovery climbs ×1.5 only
    /// through the proven-safe zone (75% of last_bad), then probes
    /// additively — never a multiplicative march back into the wall.
    #[test]
    fn congestion_memory_gates_the_reclimb() {
        // Fast reclimb below the safe zone, capped at it.
        assert_eq!(
            decide(0.0, 10.0, 0.05, 0.01, 1.0, 100.0, Some(40.0)),
            Decision::Up(15.0)
        );
        assert_eq!(
            decide(0.0, 24.0, 0.05, 0.01, 1.0, 100.0, Some(40.0)),
            Decision::Up(30.0)
        ); // capped at 40*0.75
        // At/above the safe zone: additive probing only.
        assert_eq!(
            decide(0.0, 30.0, 0.05, 0.01, 1.0, 100.0, Some(40.0)),
            Decision::Up(31.0)
        ); // + max(1, 40*0.02)
        // Large-scale controls probe proportionally (+2% of bad).
        assert_eq!(
            decide(0.0, 40_000.0, 0.05, 0.01, 1.0, 100_000.0, Some(50_000.0)),
            Decision::Up(41_000.0)
        );
    }

    /// Congestion memory clears only on a sustained streak of clean
    /// windows at-or-above the congestion point; any window without
    /// that evidence resets the streak.
    #[test]
    fn memory_clears_on_a_sustained_clean_streak() {
        assert_eq!(memory_clear_step(0, true), (1, false));
        assert_eq!(memory_clear_step(1, true), (2, false));
        assert_eq!(memory_clear_step(2, true), (0, true));
        // A breach, a dead-band hold, or a clean window still below
        // the congestion point resets the streak.
        assert_eq!(memory_clear_step(2, false), (0, false));
        assert_eq!(memory_clear_step(0, false), (0, false));
    }

    /// The dead band between `low` and `high` holds — no hunting
    /// around the operating point.
    #[test]
    fn dead_band_holds() {
        assert_eq!(
            decide(0.03, 50.0, 0.05, 0.01, 4.0, 100.0, None),
            Decision::Hold
        );
        assert_eq!(
            decide(0.03, 50.0, 0.05, 0.01, 4.0, 100.0, Some(60.0)),
            Decision::Hold
        );
    }
}
