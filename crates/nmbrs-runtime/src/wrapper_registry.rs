// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-32a — Op Wrapper Registry.
//!
//! Single source of truth for the named wrappers that compose
//! around an adapter's base dispenser: which op-template fields
//! each owns, what triggers them, what constraints they declare,
//! and how they describe their assignment to an op.
//!
//! The companion module [`crate::wrapper_resolver`] consumes the
//! registrations submitted via `inventory::submit!` to compute
//! the per-op composition order. The wrapper implementations
//! themselves live in [`crate::wrappers`] and
//! [`crate::validation`] — this module is data, not code.

use nmbrs_workload::model::{ParsedOp, WorkloadPhase};

/// Stable identifier for a wrapper. Used in workload override
/// directives, CLI flags, and registry lookups. Wrapped around a
/// `&'static str` so the value is always interned at the
/// registration site and lookups are pointer-cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WrapperName(pub &'static str);

impl WrapperName {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for WrapperName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// SRD-92 / ExecUnification — the execution-graph level(s) a wrapper is legal
/// at. Today every wrapper is op-level; the unified scaffold (Step 5) reads
/// this to place a layer at the right level once layering goes cross-level.
/// Kept decoupled from executor's WIP `ShellKind` until the unification lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperLevel {
    Op,
    Stanza,
    Phase,
    Scenario,
    Session,
}

/// SRD-82/92 — the execution unit a wrapper's trigger inspects, generalising
/// the registry across levels. An op wrapper reads the `Op` variant; a phase
/// wrapper the `Phase` variant (scenario/session variants join when those
/// levels wire up). Op-level wrappers guard with [`Self::op`]; the resolver
/// only offers a wrapper subjects of the level it declares (`applies_at`), so
/// a well-formed wrapper never sees a foreign subject — the guard is a
/// belt-and-braces `None` return.
#[derive(Clone, Copy)]
pub enum WrapperSubject<'a> {
    Op(&'a ParsedOp),
    Phase(&'a WorkloadPhase),
}

impl<'a> WrapperSubject<'a> {
    /// The execution level of this subject; pairs with [`WrapperRegistration::applies_at`].
    pub fn level(&self) -> WrapperLevel {
        match self {
            WrapperSubject::Op(_) => WrapperLevel::Op,
            WrapperSubject::Phase(_) => WrapperLevel::Phase,
        }
    }

    /// The op template, if this is an op subject (op-wrapper triggers guard on it).
    pub fn op(&self) -> Option<&'a ParsedOp> {
        match self {
            WrapperSubject::Op(op) => Some(op),
            _ => None,
        }
    }

    /// The phase, if this is a phase subject (phase-wrapper triggers guard on it).
    pub fn phase(&self) -> Option<&'a WorkloadPhase> {
        match self {
            WrapperSubject::Phase(p) => Some(p),
            _ => None,
        }
    }

    /// Uniform "is this wrapper-owned field present?" — the canonical field
    /// names mapped to each unit's actual storage. Op fields are spread
    /// across `params`/`condition`/`delay`/`rate`; phase fields are typed
    /// slots. Used by the parse-time misplaced-field guard.
    pub fn has_owned_field(&self, field: &str) -> bool {
        match self {
            WrapperSubject::Op(op) => match field {
                "if" => op.condition.is_some(),
                "delay" => op.delay.is_some(),
                "while" => op.while_cond.is_some(),
                "rate" => op.rate.is_some(),
                _ => op.params.contains_key(field),
            },
            WrapperSubject::Phase(p) => match field {
                "interval" => p.interval.is_some(),
                "repeat" => p.repeat.is_some(),
                _ => false,
            },
        }
    }
}

/// One entry per registered wrapper. Entries are submitted at
/// link time via `inventory::submit!` and collected at startup
/// into the [`WrapperRegistry`] view.
///
/// The fields here are pure declaration: which fields the
/// wrapper consumes, when it applies, what relationships it
/// has to other wrappers, and how it describes its assignment.
/// Construction of the dispenser layer itself is NOT in the
/// registration — the cascade in `activity.rs` continues to
/// hold the per-wrapper `wrap()` calls (each has a different
/// signature). The registry decides PRESENCE and ORDER; the
/// cascade looks up the resolved plan and dispatches by name.
pub struct WrapperRegistration {
    /// Stable name (`"validate"`, `"poll"`, `"delay"`,
    /// `"if"`, `"emit"`, `"result"`, `"metrics"`, `"traverse"`).
    pub name: WrapperName,

    /// Op-template field names this wrapper exclusively owns.
    /// Listed for parse-time validation: a misplaced field like
    /// `poll_interval_ms: 5000` on an op without `poll:` becomes
    /// a hard error pointing at THIS registration, not an opaque
    /// "unknown param".
    ///
    /// Pure data — the parse-time guard reads this; the wrapper
    /// implementation reads its own fields directly off the
    /// `ParsedOp`.
    pub owned_fields: &'static [&'static str],

    /// Predicate over the op template: "does this wrapper apply
    /// to this op?" Default behaviour: any owned field present.
    /// Wrappers with no owned fields (e.g. `result`, which fires
    /// whenever the op declares any `result:` wires) override
    /// this with their own logic.
    pub triggers: fn(WrapperSubject) -> bool,

    /// Wrappers that MUST sit inside this one (closer to the
    /// adapter, called *after* this one per cycle).
    /// Activates transitively: triggering `validate` pulls in
    /// `traverse` whether or not a traverse field was declared.
    pub requires_inner: &'static [WrapperName],

    /// Wrappers that MUST NOT sit outside this one. Hard error
    /// when the constraint graph would permit any of the listed
    /// wrappers to wrap this one.
    pub forbids_outer: &'static [WrapperName],

    /// Wrappers that cannot coexist with this one on a given
    /// op. Triggering both is a hard error.
    pub mutually_exclusive_with: &'static [WrapperName],

    /// One-line summary of what this wrapper, configured for
    /// the given op template, will do at runtime.
    /// Emitted at Info level once per op-template activation,
    /// alongside the other wrappers in the resolved plan.
    ///
    /// Examples:
    /// - `validate`: `"validate: min_rows ≥ 1 (strict)"`
    /// - `poll`:     `"poll: every 5s, timeout 600s, on \`await_empty\`"`
    /// - `if`:       `"if: cql_dialect == 'cass5'"`
    ///
    /// Returns `None` for wrappers that have nothing useful to
    /// say (e.g. an always-on `traverse` with no per-op
    /// configuration); operators see the wrappers that actually
    /// shape behaviour, the boilerplate stays at Debug.
    ///
    /// Distinct from `OpDispenser::describe()` which describes
    /// the *runtime op* — e.g. the CQL statement text — for
    /// error-context dumps. This describes the *wrapper's
    /// contribution* for init-time diagnostics.
    pub describe_assignment: fn(WrapperSubject) -> Option<String>,

    /// SRD-92 / ExecUnification — the execution-graph level(s) this wrapper is
    /// legal at. Every current wrapper is `&[WrapperLevel::Op]`; the unified
    /// scaffold reads this to know where a layer may sit. Carried as metadata
    /// now; consumed when layering goes cross-level (Step 5).
    pub levels: &'static [WrapperLevel],
}

inventory::collect!(WrapperRegistration);

impl WrapperRegistration {
    /// SRD-92 — whether this wrapper is legal at `level`.
    pub fn applies_at(&self, level: WrapperLevel) -> bool {
        self.levels.contains(&level)
    }
}

#[cfg(test)]
mod level_tests {
    use super::*;

    fn no_trigger(_: WrapperSubject) -> bool {
        false
    }
    fn no_describe(_: WrapperSubject) -> Option<String> {
        None
    }

    #[test]
    fn applies_at_reads_declared_levels() {
        let reg = WrapperRegistration {
            name: WrapperName::new("t"),
            owned_fields: &[],
            triggers: no_trigger,
            requires_inner: &[],
            forbids_outer: &[],
            mutually_exclusive_with: &[],
            describe_assignment: no_describe,
            levels: &[WrapperLevel::Op, WrapperLevel::Phase],
        };
        assert!(reg.applies_at(WrapperLevel::Op));
        assert!(reg.applies_at(WrapperLevel::Phase));
        assert!(!reg.applies_at(WrapperLevel::Session));
    }
}

/// Live view over every registered wrapper. Built once at
/// startup from the `inventory` collection.
///
/// The struct is cheap to clone (it borrows the static
/// registrations) and is passed to the [`crate::wrapper_resolver::WrapperResolver`]
/// for per-op-template plan computation.
pub struct WrapperRegistry {
    entries: Vec<&'static WrapperRegistration>,
}

impl WrapperRegistry {
    /// Build the registry from every `inventory::submit!`
    /// entry currently linked into the binary. Invoked once
    /// at startup.
    pub fn from_inventory() -> Self {
        let mut entries: Vec<&'static WrapperRegistration> =
            inventory::iter::<WrapperRegistration>().collect();
        entries.sort_by_key(|r| r.name);
        Self { entries }
    }

    /// Iterate every registered wrapper.
    pub fn iter(&self) -> impl Iterator<Item = &'static WrapperRegistration> + '_ {
        self.entries.iter().copied()
    }

    /// Whether `field` is an op-template key declared as owned by ANY
    /// registered wrapper (its trigger or a knob it consumes). This is
    /// the single structural source of truth for "this params key is a
    /// wrapper field" — driven by each wrapper's `owned_fields`
    /// declaration, not a hand-maintained parallel list. The op
    /// closed-vocabulary guard consults it so a wrapper field is
    /// accepted because a wrapper *declares* it, not because it happens
    /// to also be a CLI param (SRD-32a). Adding a wrapper therefore
    /// never requires touching `CORE_OP_PARAMS` or the CLI vocabulary.
    pub fn owns_field(&self, field: &str) -> bool {
        self.iter().any(|reg| reg.owned_fields.contains(&field))
    }

    /// Every field name owned by some registered wrapper, deduplicated.
    /// Exposed for diagnostics (the closed-vocab guard names the full
    /// accepted wrapper vocabulary) and for the drift-guard test.
    pub fn all_owned_fields(&self) -> std::collections::BTreeSet<&'static str> {
        self.iter()
            .flat_map(|reg| reg.owned_fields.iter().copied())
            .collect()
    }

    /// Look up by name. Returns `None` for unknown names; the
    /// caller surfaces that as a typo diagnostic with a
    /// closest-match suggestion (see [`closest_match`]).
    pub fn get(&self, name: WrapperName) -> Option<&'static WrapperRegistration> {
        self.entries.iter().copied().find(|r| r.name == name)
    }

    /// Look up by raw string. Convenience for parsing the
    /// `--wrap-default-order` / `wrappers.order` lists.
    pub fn get_str(&self, name: &str) -> Option<&'static WrapperRegistration> {
        self.entries
            .iter()
            .copied()
            .find(|r| r.name.as_str() == name)
    }

    /// Number of registered wrappers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Find the registered wrapper name closest to `query` by
    /// Levenshtein distance. Used for "did you mean … ?"
    /// diagnostics on unknown names.
    pub fn closest_match(&self, query: &str) -> Option<&'static str> {
        closest_match(query, self.entries.iter().map(|r| r.name.as_str()))
    }

    /// SRD-32a Push 2 — find every owned field that's present
    /// on the op template but whose owning wrapper does NOT
    /// trigger. Returns one (wrapper, field) pair per
    /// violation; an empty vec means every field is in its
    /// proper place.
    ///
    /// `field_present_on_template` is the predicate the
    /// caller uses to ask "is field X set on this op?" — it
    /// abstracts the fact that some wrapper-owned fields
    /// (e.g. `if`, `delay`) live outside `params:`. In
    /// practice the owned fields that AREN'T also their
    /// wrapper's trigger always live under `params:`, so a
    /// caller can pass a closure over `template.params
    /// .contains_key`.
    pub fn misplaced_fields(&self, subject: WrapperSubject) -> Vec<(WrapperName, &'static str)> {
        let mut out: Vec<(WrapperName, &'static str)> = Vec::new();
        for reg in self.iter() {
            // Only wrappers legal at this subject's level can claim its
            // fields; and a triggered wrapper's fields are correctly placed.
            if !reg.applies_at(subject.level()) || (reg.triggers)(subject) {
                continue;
            }
            for &field in reg.owned_fields {
                if subject.has_owned_field(field) {
                    out.push((reg.name, field));
                }
            }
        }
        out
    }
}

/// Find the closest match in a list of candidate names, using
/// Levenshtein edit distance. Returns `None` when no candidate
/// is within distance 3, since beyond that the suggestion is
/// noise.
pub fn closest_match<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let mut best: Option<(&str, usize)> = None;
    for c in candidates {
        let d = levenshtein(query, c);
        match best {
            Some((_, prev)) if d >= prev => {}
            _ => best = Some((c, d)),
        }
    }
    best.filter(|&(_, d)| d <= 3).map(|(s, _)| s)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("validate", "validatte"), 1);
    }

    #[test]
    fn closest_match_finds_typo() {
        let names = ["validate", "poll", "delay"];
        assert_eq!(closest_match("validatte", names), Some("validate"));
        assert_eq!(closest_match("plll", names), Some("poll"));
        assert_eq!(closest_match("wildly_different", names), None);
    }
}
