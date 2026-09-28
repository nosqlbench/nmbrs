// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Outcome instrument — a success/failure-style op measurement whose
//! detail level is chosen at construction (SRD-91).
//!
//! Every op-outcome instrument (attempt / result × success / failure)
//! tracks at least a count. The count is the load-bearing quantity for
//! the SRD-91 cross-check invariants. Whether the op DURATION is also
//! retained (as an HDR distribution) is a per-family, config-driven
//! choice:
//!
//! - [`MetricDetail::Counts`] → a plain [`Counter`]; cheap.
//! - [`MetricDetail::Timers`] → a full [`Timer`] (count + sum +
//!   quantiles).
//!
//! Because a [`Timer`] exposes its observation count, the count — and
//! therefore every count-based invariant — is identical in both modes;
//! the mode only decides whether the distribution is kept. The
//! recording site always calls [`OutcomeInstrument::observe`] with the
//! op duration; in Counts mode the duration is dropped.

use crate::component::InstrumentRef;
use crate::instruments::counter::Counter;
use crate::instruments::timer::Timer;
use crate::labels::Labels;
use std::collections::HashMap;
use std::sync::Arc;

/// Detail level for an [`OutcomeInstrument`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetricDetail {
    /// Count only — exported as a `counter` family.
    Counts,
    /// Count + duration distribution — exported as a `summary` family.
    /// Default, preserving the historical always-on timers.
    #[default]
    Timers,
}

impl MetricDetail {
    /// Parse `"counts"` / `"timers"` (case-insensitive, with a few
    /// natural synonyms). Returns `None` for anything else so callers
    /// decide whether to error or fall back.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "counts" | "count" | "counter" | "counters" => Some(Self::Counts),
            "timers" | "timer" | "timed" | "histogram" => Some(Self::Timers),
            _ => None,
        }
    }
}

/// Per-family detail selection: a global default plus per-family-name
/// overrides (SRD-91 "global default + per-family override"). A `None`
/// override falls through to the default.
#[derive(Debug, Clone, Default)]
pub struct MetricDetailConfig {
    default: MetricDetail,
    overrides: HashMap<String, MetricDetail>,
}

impl MetricDetailConfig {
    /// Config with a global default and no overrides.
    pub fn new(default: MetricDetail) -> Self {
        Self {
            default,
            overrides: HashMap::new(),
        }
    }

    /// Builder-style per-family override.
    pub fn with_override(mut self, family: impl Into<String>, detail: MetricDetail) -> Self {
        self.overrides.insert(family.into(), detail);
        self
    }

    /// The detail mode for a metric family name.
    pub fn for_family(&self, family: &str) -> MetricDetail {
        self.overrides.get(family).copied().unwrap_or(self.default)
    }

    /// The global default mode.
    pub fn default_detail(&self) -> MetricDetail {
        self.default
    }
}

/// A success/failure outcome instrument whose backing instrument is
/// fixed at construction by a [`MetricDetail`]. See module docs.
pub enum OutcomeInstrument {
    Counted(Arc<Counter>),
    Timed(Arc<Timer>),
}

impl OutcomeInstrument {
    /// Build in the requested detail mode. `sigdigs` is only used in
    /// [`MetricDetail::Timers`] mode (HDR precision).
    pub fn new(labels: Labels, sigdigs: u8, detail: MetricDetail) -> Self {
        match detail {
            MetricDetail::Counts => Self::Counted(Arc::new(Counter::new(labels))),
            MetricDetail::Timers => Self::Timed(Arc::new(Timer::with_sigdigs(labels, sigdigs))),
        }
    }

    /// Record one outcome with its duration. In Counts mode the
    /// duration is ignored (the count increments by one).
    pub fn observe(&self, duration_nanos: u64) {
        match self {
            Self::Counted(c) => c.inc(),
            Self::Timed(t) => t.record(duration_nanos),
        }
    }

    /// Observation count — defined identically in both modes.
    pub fn count(&self) -> u64 {
        match self {
            Self::Counted(c) => c.get(),
            Self::Timed(t) => t.count(),
        }
    }

    /// Labels (including the `name`) of the backing instrument.
    pub fn labels(&self) -> &Labels {
        match self {
            Self::Counted(c) => c.labels(),
            Self::Timed(t) => t.labels(),
        }
    }

    /// `true` when this instrument retains the duration distribution.
    pub fn is_timed(&self) -> bool {
        matches!(self, Self::Timed(_))
    }

    /// The registry handle for this instrument's current mode, for
    /// [`crate::component::Component::register_instrument`].
    pub fn instrument_ref(&self) -> InstrumentRef {
        match self {
            Self::Counted(c) => InstrumentRef::Counter(c.clone()),
            Self::Timed(t) => InstrumentRef::Timer(t.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_mode_is_counter_with_count() {
        let i = OutcomeInstrument::new(
            Labels::of("name", "result_success"),
            3,
            MetricDetail::Counts,
        );
        assert!(!i.is_timed());
        i.observe(1234);
        i.observe(5678);
        assert_eq!(i.count(), 2);
    }

    #[test]
    fn timed_mode_keeps_count_and_distribution() {
        let i = OutcomeInstrument::new(
            Labels::of("name", "result_success"),
            3,
            MetricDetail::Timers,
        );
        assert!(i.is_timed());
        i.observe(1_000_000);
        i.observe(2_000_000);
        assert_eq!(i.count(), 2);
    }

    #[test]
    fn detail_parse_and_config_override() {
        assert_eq!(MetricDetail::parse("counts"), Some(MetricDetail::Counts));
        assert_eq!(MetricDetail::parse("Timers"), Some(MetricDetail::Timers));
        assert_eq!(MetricDetail::parse("nope"), None);
        let cfg = MetricDetailConfig::new(MetricDetail::Timers)
            .with_override("attempt_success", MetricDetail::Counts);
        assert_eq!(cfg.for_family("attempt_success"), MetricDetail::Counts);
        assert_eq!(cfg.for_family("result_success"), MetricDetail::Timers);
    }
}
