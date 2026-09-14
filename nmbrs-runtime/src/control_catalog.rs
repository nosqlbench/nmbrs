// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-23 — the **dynamic-control capability catalog**.
//!
//! Controls are declared *imperatively* into the live component tree at run
//! time (`Component::controls().declare(...)`), which makes them enumerable
//! through that tree once it exists — `dryrun=controls`, SRD-23
//! §"Enumeration". But that only surfaces the controls a given run *happened*
//! to declare: anything conditional (`rate`, only when a phase sets `rate:`)
//! or adapter-owned (`cql_trace_rate`, only when the CQL adapter is active) is
//! invisible until its conditions are met. That asymmetry is why
//! `concurrency` (always declared) feels discoverable and `rate` does not.
//!
//! This module adds the complementary **capability tier**: a static,
//! pre-instantiation description of every control the binary *can* declare,
//! readable by `nmbrs describe controls` without running anything. Each
//! [`ControlDesc`] is the **single source of truth** — the imperative
//! declaration *derives* its name / value-type / default / range / gauge from
//! the descriptor (via [`ControlDesc::build_u32`] / [`ControlDesc::build_f64`]
//! / [`ControlDesc::build_rate`]), so the discovery surface and the live
//! control can never drift.
//!
//! Owners: core controls (`concurrency`, `rate`) live here; adapter controls
//! are contributed by each adapter's
//! [`supported_controls`](crate::adapter::AdapterRegistration::supported_controls)
//! and unioned in by [`all_controls`].

use nmbrs_metrics::controls::{BranchScope, Control, ControlBuilder};

/// The value shape of a control, as projected onto the f64-writable surface
/// every writer (TUI `e`, `POST /controls`, polydat `control_set`,
/// `optimize.servo`) shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlValueType {
    /// Unsigned integer count (e.g. `concurrency` — fibers). Written as an
    /// f64, range-checked and truncated to `u32` on apply.
    Count,
    /// Throughput in operations per second (e.g. `rate`). Backed by a
    /// `RateSpec`; the f64 surface is ops/sec.
    Rate,
    /// A probability / fraction in `[min, max]` (e.g. `cql_trace_rate`).
    Fraction,
    /// An event-frequency ceiling in events/sec (e.g.
    /// `retry_exemplar_max_hz`). `0` = uncapped.
    Frequency,
}

impl ControlValueType {
    /// Short human label for `describe` tables.
    pub fn label(self) -> &'static str {
        match self {
            ControlValueType::Count => "count(u32)",
            ControlValueType::Rate => "rate(ops/s)",
            ControlValueType::Fraction => "fraction",
            ControlValueType::Frequency => "freq(hz)",
        }
    }
}

/// The condition under which a control is actually declared on the component
/// tree — the *why isn't this knob here?* a user needs when a control is
/// absent from a given run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredWhen {
    /// Declared on every activity, unconditionally.
    Always,
    /// Declared when a phase/op sets the named field (e.g. `rate` ⇒ the
    /// `rate:` phase field both seeds and declares the `rate` control).
    PhaseField(&'static str),
    /// Declared when the named adapter is active in the run.
    AdapterActive(&'static str),
    /// Declared only when the named *driver* implementation backs its adapter
    /// (e.g. `cassandra-cpp` for the `cql` adapter — the pure-Rust `scylla`
    /// driver does not declare it). Adapter-owned but driver-specific.
    Driver(&'static str),
}

impl std::fmt::Display for DeclaredWhen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeclaredWhen::Always => write!(f, "always"),
            DeclaredWhen::PhaseField(field) => write!(f, "when a phase sets `{field}:`"),
            DeclaredWhen::AdapterActive(name) => write!(f, "when the `{name}` adapter is active"),
            DeclaredWhen::Driver(driver) => write!(f, "when the `{driver}` driver is active"),
        }
    }
}

/// Static, declarative description of one dynamic control's *capability*. See
/// the module docs: this is the single source the imperative declaration
/// derives from and that `describe controls` reads, so they cannot drift.
#[derive(Debug, Clone, Copy)]
pub struct ControlDesc {
    /// Canonical control name — the key written via `control_set`, the TUI
    /// `e` prompt, `GET`/`POST /controls`, and `optimize.servo`.
    pub name: &'static str,
    /// Value shape (drives the derived gauge + f64 conversion).
    pub value_type: ControlValueType,
    /// Documented default, projected to f64 (the baseline a run starts from
    /// when the field is unset; informational for discovery).
    pub default: f64,
    /// Inclusive lower bound the derived `from_f64` validator enforces. For
    /// [`ControlValueType::Rate`] this is treated as an *exclusive* floor (a
    /// rate of `0` disables the limiter rather than servoing it).
    pub min: f64,
    /// Inclusive upper bound the derived `from_f64` validator enforces.
    pub max: f64,
    /// Unit label for `describe` (e.g. `fibers`, `ops/sec`, `probability`).
    pub unit: &'static str,
    /// One-line description of what the knob does.
    pub doc: &'static str,
    /// When the control is actually declared on the component tree.
    pub declared_when: DeclaredWhen,
}

impl ControlDesc {
    /// Derive a `u32` count control from this descriptor — name, `[min, max]`
    /// range validator, and gauge projection all come from the descriptor —
    /// seeded at `initial`. The caller attaches the instance-specific applier
    /// (e.g. the fiber-pool resize) separately.
    pub fn build_u32(&self, initial: u32) -> Control<u32> {
        debug_assert_eq!(
            self.value_type,
            ControlValueType::Count,
            "{} is not a Count control",
            self.name
        );
        let (name, min, max) = (self.name, self.min, self.max);
        ControlBuilder::new(name, initial)
            .reify_as_gauge(|v: &u32| Some(*v as f64))
            .from_f64(move |v| {
                if !v.is_finite() || !(min..=max).contains(&v) {
                    Err(format!("{name} out of range [{min}, {max}]: got {v}"))
                } else {
                    Ok(v as u32)
                }
            })
            .branch_scope(BranchScope::Local)
            .build()
    }

    /// Derive an `f64` fraction/frequency control from this descriptor.
    pub fn build_f64(&self, initial: f64) -> Control<f64> {
        debug_assert!(
            matches!(
                self.value_type,
                ControlValueType::Fraction | ControlValueType::Frequency
            ),
            "{} is not an f64-shaped control",
            self.name
        );
        let (name, min, max) = (self.name, self.min, self.max);
        ControlBuilder::new(name, initial)
            .reify_as_gauge(|v: &f64| Some(*v))
            .from_f64(move |v| {
                if !v.is_finite() || !(min..=max).contains(&v) {
                    Err(format!("{name} must be in [{min}, {max}]: got {v}"))
                } else {
                    Ok(v)
                }
            })
            .branch_scope(BranchScope::Local)
            .build()
    }

    /// Derive a `RateSpec` throughput control from this descriptor. `min` is an
    /// *exclusive* floor: a non-positive value is rejected (a zero rate
    /// disables the limiter, which is a phase-config decision, not a servo).
    pub fn build_rate(&self, initial_ops_per_sec: f64) -> Control<nmbrs_rate::RateSpec> {
        debug_assert_eq!(
            self.value_type,
            ControlValueType::Rate,
            "{} is not a Rate control",
            self.name
        );
        let name = self.name;
        ControlBuilder::new(name, nmbrs_rate::RateSpec::new(initial_ops_per_sec))
            .reify_as_gauge(|spec: &nmbrs_rate::RateSpec| Some(spec.ops_per_sec))
            .from_f64(move |v| {
                if v <= 0.0 {
                    Err(format!("{name} must be > 0, got {v}"))
                } else {
                    Ok(nmbrs_rate::RateSpec::new(v))
                }
            })
            .branch_scope(BranchScope::Local)
            .build()
    }
}

/// `concurrency` — the fiber count the executor maintains for a phase. Always
/// declared, so it is the one control present in every run.
pub const CONCURRENCY: ControlDesc = ControlDesc {
    name: "concurrency",
    value_type: ControlValueType::Count,
    default: 1.0,
    min: 1.0,
    max: 100_000.0,
    unit: "fibers",
    doc: "Concurrent fibers the executor maintains; re-balanced live on every change.",
    declared_when: DeclaredWhen::Always,
};

/// `rate` — the cycle-rate limiter (ops/sec). Declared only when a phase sets
/// `rate:`; that field value seeds the control (and is the warmup an
/// `optimize.servo: rate` retargets from).
pub const RATE: ControlDesc = ControlDesc {
    name: "rate",
    value_type: ControlValueType::Rate,
    default: 0.0,
    min: 0.0,
    max: f64::INFINITY,
    unit: "ops/sec",
    doc: "Target cycle rate (ops/sec) enforced by the rate limiter.",
    declared_when: DeclaredWhen::PhaseField("rate"),
};

/// `retry_exemplar_rate` — retry-error counter-exemplar sampling
/// (SRD-82 Part 3b / `exec_events`). Push-on-set: the applier is one
/// atomic store into the activity's shared `ExemplarConfig`; samplers
/// read it only on their retry path. Ops that pin `retry_exemplar_*`
/// params hold private cells this control does not move.
pub const RETRY_EXEMPLAR_RATE: ControlDesc = ControlDesc {
    name: "retry_exemplar_rate",
    value_type: ControlValueType::Fraction,
    default: 0.0,
    min: 0.0,
    max: 1.0,
    unit: "probability",
    doc: "Fraction of retried op errors sampled to the session log as counter-exemplars (0 = off, 1 = all); moves every op without a pinned retry_exemplar_* param.",
    declared_when: DeclaredWhen::Always,
};

/// `retry_exemplar_max_hz` — emission-frequency ceiling for retry
/// exemplars; admissions over it are squelched and counted (reported
/// on the next emitted line). `0` = uncapped.
pub const RETRY_EXEMPLAR_MAX_HZ: ControlDesc = ControlDesc {
    name: "retry_exemplar_max_hz",
    value_type: ControlValueType::Frequency,
    default: 5.0,
    min: 0.0,
    max: 1_000_000.0,
    unit: "events/sec",
    doc: "Ceiling on retry-exemplar emissions per second; excess is squelched and counted, never silent. 0 = uncapped.",
    declared_when: DeclaredWhen::Always,
};

/// The core controls every build supports (subject to each one's
/// [`DeclaredWhen`] condition). Adapter controls are added by [`all_controls`].
pub fn core_controls() -> &'static [ControlDesc] {
    const CORE: &[ControlDesc] = &[
        CONCURRENCY,
        RATE,
        RETRY_EXEMPLAR_RATE,
        RETRY_EXEMPLAR_MAX_HZ,
    ];
    CORE
}

/// Where a control's descriptor is contributed from — shown by
/// `describe controls` so a user knows which subsystem owns the knob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOwner {
    /// Core activity/runtime control (always compiled in).
    Core,
    /// Contributed by an adapter registration (its primary driver name).
    Adapter(String),
}

impl std::fmt::Display for ControlOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlOwner::Core => write!(f, "core"),
            ControlOwner::Adapter(name) => write!(f, "adapter:{name}"),
        }
    }
}

/// One row of the capability catalog: a descriptor plus its owner.
pub struct ControlEntry {
    /// The static capability description.
    pub desc: &'static ControlDesc,
    /// Which subsystem contributes it.
    pub owner: ControlOwner,
}

/// Enumerate **every** control the binary can declare — core plus each
/// registered adapter's [`supported_controls`](crate::adapter::AdapterRegistration::supported_controls)
/// — for `nmbrs describe controls`. This is the static *capability* view
/// (SRD-23 §"Enumeration"), distinct from the *instance* view `dryrun=controls`
/// walks over the live component tree.
pub fn all_controls() -> Vec<ControlEntry> {
    let mut out: Vec<ControlEntry> = core_controls()
        .iter()
        .map(|d| ControlEntry {
            desc: d,
            owner: ControlOwner::Core,
        })
        .collect();
    for reg in inventory::iter::<crate::adapter::AdapterRegistration> {
        let owner_name = (reg.names)().first().copied().unwrap_or("?");
        for d in (reg.supported_controls)() {
            out.push(ControlEntry {
                desc: d,
                owner: ControlOwner::Adapter(owner_name.to_string()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_metrics::controls::ControlOrigin;

    #[test]
    fn core_controls_cover_concurrency_and_rate_with_conditions() {
        let core = core_controls();
        let conc = core
            .iter()
            .find(|d| d.name == "concurrency")
            .expect("concurrency present");
        let rate = core
            .iter()
            .find(|d| d.name == "rate")
            .expect("rate present");
        // The asymmetry the catalog fixes: concurrency is always present, rate
        // only when a phase sets `rate:` — both now *discoverable* statically.
        assert_eq!(conc.declared_when, DeclaredWhen::Always);
        assert_eq!(rate.declared_when, DeclaredWhen::PhaseField("rate"));
        assert_eq!(conc.value_type, ControlValueType::Count);
        assert_eq!(rate.value_type, ControlValueType::Rate);
    }

    #[tokio::test]
    async fn build_u32_derives_name_and_range_from_descriptor() {
        use nmbrs_metrics::controls::ErasedControl;
        // The live control is *derived* from the descriptor — its name and the
        // range its `from_f64` enforces come from `CONCURRENCY`, not a separate
        // literal, so the discovery surface and the knob can't drift.
        let ctl = CONCURRENCY.build_u32(8);
        assert_eq!(ctl.name(), "concurrency");
        assert_eq!(ctl.value(), 8);
        // In-range write applies; out-of-range (below min=1) is rejected by the
        // derived validator.
        assert!(ctl.set_f64(16.0, ControlOrigin::Launch).await.is_ok());
        assert_eq!(ctl.value(), 16);
        assert!(ctl.set_f64(0.0, ControlOrigin::Launch).await.is_err());
    }

    #[tokio::test]
    async fn build_rate_rejects_non_positive() {
        use nmbrs_metrics::controls::ErasedControl;
        let ctl = RATE.build_rate(1000.0);
        assert_eq!(ctl.name(), "rate");
        assert!(ctl.set_f64(500.0, ControlOrigin::Launch).await.is_ok());
        assert!(ctl.set_f64(0.0, ControlOrigin::Launch).await.is_err());
    }

    #[test]
    fn all_controls_includes_core_and_is_owner_tagged() {
        let all = all_controls();
        let conc = all
            .iter()
            .find(|e| e.desc.name == "concurrency")
            .expect("concurrency enumerated");
        assert_eq!(conc.owner, ControlOwner::Core);
    }
}
