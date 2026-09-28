// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-32a — Wrapper composition resolver.
//!
//! Turns a parsed op template + the wrapper registry + the
//! session-level default order into a concrete
//! [`WrapperPlan`]: which wrappers to apply, in what order,
//! with provenance tagging for diagnostics.
//!
//! Algorithm (4 passes, see SRD-32a §"Algorithm"):
//! 1. Trigger fan-out — every wrapper whose `triggers(template)`
//!    returns true is added with `OwnedField` provenance.
//! 2. Transitive closure — close `requires_inner` edges,
//!    tagging additions with `TransitiveFrom`.
//! 3. Constraint validation — `mutually_exclusive_with`
//!    pairs and `requires_inner` cycles surface as errors.
//! 4. Topological order — innermost first, with the
//!    session-level default order breaking ties; then a
//!    `forbids_outer` post-scan rejects any ordering that
//!    placed a forbidden wrapper outside its inner.

use std::collections::{HashMap, HashSet};

use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperRegistry, WrapperSubject};

/// Resolved wrapper composition for one op template.
pub struct WrapperPlan {
    /// Wrappers in composition order — innermost first
    /// (built first; called last per cycle), outermost
    /// last. The executor applies them in this order to
    /// construct the final dispenser chain.
    pub stack: Vec<&'static WrapperRegistration>,

    /// Diagnostic record of which wrappers triggered
    /// directly vs. via transitive activation. Used by
    /// `nmbrs describe op` to explain why each wrapper
    /// is present.
    pub provenance: Vec<WrapperActivation>,
}

impl WrapperPlan {
    /// Iterate the stack innermost-to-outermost.
    pub fn iter_innermost_first(&self) -> impl Iterator<Item = &'static WrapperRegistration> + '_ {
        self.stack.iter().copied()
    }

    /// Find the activation record for a wrapper name. Returns
    /// `None` if the wrapper isn't in the plan.
    pub fn activation(&self, name: WrapperName) -> Option<&WrapperActivation> {
        self.provenance.iter().find(|a| a.wrapper() == name)
    }
}

/// How a wrapper came to be in a plan — directly triggered
/// by a field, transitively pulled in by another wrapper, or
/// always-on.
#[derive(Debug, Clone)]
pub enum WrapperActivation {
    /// Triggered directly by an owned field on the op
    /// template (e.g. `validate` because `verify:` was
    /// declared).
    OwnedField {
        wrapper: WrapperName,
        field: &'static str,
    },
    /// Pulled in transitively by another wrapper's
    /// `requires_inner`.
    TransitiveFrom {
        wrapper: WrapperName,
        requested_by: WrapperName,
    },
    /// Always-on wrapper (e.g. `traverse`, `result`).
    AlwaysOn { wrapper: WrapperName },
}

impl WrapperActivation {
    pub fn wrapper(&self) -> WrapperName {
        match self {
            Self::OwnedField { wrapper, .. } => *wrapper,
            Self::TransitiveFrom { wrapper, .. } => *wrapper,
            Self::AlwaysOn { wrapper } => *wrapper,
        }
    }
}

/// Errors the resolver may surface. Each variant carries
/// enough context for the caller to render an actionable
/// diagnostic without re-walking the registry.
#[derive(Debug)]
pub enum ResolveError {
    /// `forbids_outer` violation — wrapper `inner` declared
    /// `outer` must not wrap it, but the resolved order
    /// placed `outer` outside `inner`.
    ForbiddenOuter {
        inner: WrapperName,
        outer: WrapperName,
    },
    /// `mutually_exclusive_with` violation — both triggered
    /// for the same op.
    MutuallyExclusive {
        a: WrapperName,
        b: WrapperName,
        a_reason: WrapperActivation,
        b_reason: WrapperActivation,
    },
    /// Constraint graph contains a `requires_inner` cycle
    /// (e.g. A.requires_inner = [B] and B.requires_inner =
    /// [A]). Almost always a registry-author bug; surface
    /// at session start.
    ConstraintCycle { cycle: Vec<WrapperName> },
    /// Override referenced an unknown wrapper name. Carries
    /// the closest registered name as a typo suggestion when
    /// available.
    UnknownWrapper {
        name: String,
        suggestion: Option<&'static str>,
    },
    /// `requires_inner` pointed at a wrapper that doesn't
    /// exist in the registry. Almost always a registry-author
    /// bug.
    DanglingRequiresInner {
        from: WrapperName,
        missing: WrapperName,
    },
    /// SRD-32a Push 3 — an explicit `wrappers: { order: [...] }`
    /// override is not a permutation of the wrappers triggered
    /// on the op. Either a triggered wrapper is missing from
    /// the override (`missing`), or the override names a
    /// wrapper whose trigger doesn't fire (`extra`). Exactly
    /// one of `missing`/`extra` is set per error; the resolver
    /// reports the first violation it finds.
    OverridePermutationMismatch {
        missing: Option<WrapperName>,
        extra: Option<WrapperName>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForbiddenOuter { inner, outer } => write!(
                f,
                "wrapper `{outer}` was placed outside `{inner}`, \
                 which is forbidden by `{inner}`'s constraint graph",
            ),
            Self::MutuallyExclusive { a, b, .. } => write!(
                f,
                "wrappers `{a}` and `{b}` are mutually exclusive but \
                 both triggered on this op",
            ),
            Self::ConstraintCycle { cycle } => {
                write!(f, "wrapper requires_inner cycle: ")?;
                for (i, n) in cycle.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" → ")?;
                    }
                    write!(f, "{n}")?;
                }
                Ok(())
            }
            Self::UnknownWrapper { name, suggestion } => {
                write!(f, "unknown wrapper `{name}`")?;
                if let Some(s) = suggestion {
                    write!(f, "; did you mean `{s}`?")?;
                }
                Ok(())
            }
            Self::DanglingRequiresInner { from, missing } => write!(
                f,
                "wrapper `{from}` declares requires_inner=[{missing}] but `{missing}` is not registered",
            ),
            Self::OverridePermutationMismatch { missing, extra } => {
                if let Some(m) = missing {
                    write!(
                        f,
                        "wrapper override is missing triggered wrapper `{m}` — \
                         every wrapper that fires on this op must appear in \
                         `wrappers: {{ order: [...] }}`"
                    )
                } else if let Some(e) = extra {
                    write!(
                        f,
                        "wrapper override names `{e}`, but its trigger \
                         condition is not satisfied for this op — remove it \
                         from `wrappers: {{ order: [...] }}` or add the \
                         trigger field that activates it"
                    )
                } else {
                    write!(f, "wrapper override permutation mismatch")
                }
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Default composition order (innermost → outermost). Used
/// by the resolver as a tiebreaker when constraints leave
/// multiple valid orderings.
///
/// Matches the cascade hand-rolled in `activity.rs` today;
/// tests pin this exact sequence to keep the migration
/// byte-identical.
pub const DEFAULT_ORDER: &[&str] = &[
    // `tries` is the ABSOLUTE INNERMOST layer (SRD-82 Part 3b): it wraps the
    // raw adapter dispenser directly, owning the attempt loop + per-attempt
    // panic catch. Hand-placed in the cascade (before the plan loop); this
    // slot keeps the PLAN's order aligned with the runtime truth when an
    // op's `tries:` field puts it in the plan.
    "tries", "traverse", "delay", "validate", "poll", "if", "result", "metrics",
    // `memo` must appear before `dryrun` here so the topo-
    // sort tiebreak places it INSIDE dryrun. Wrappers absent
    // from this list get `order_index = usize::MAX` and
    // therefore lose every tiebreak — which puts them at the
    // outermost position. That's harmless when no wrapper
    // forbids them being outside, but dryrun's
    // `forbids_outer` set lists memo (dryrun must be the
    // absolute outermost short-circuit), so memo missing
    // from default order would surface as a
    // `ForbiddenOuter` resolve error.
    "memo",
    // `gutter` sits beside memo for the same reason memo needs a
    // slot: dryrun forbids it outer, so it must beat dryrun's
    // tiebreak. Inside `while:` / `rate:` so each loop
    // iteration publishes its own cell value.
    "gutter",
    // `rate:` paces inner iterations. Slotted inside
    // `while:` so the acquire fires once per loop iteration;
    // outside metrics/memo so the wait isn't counted against
    // per-op service-time measurement.
    "rate",
    // `while:` sits outer of memo + metrics so each loop
    // iteration's per-cycle effects (metric update, memo
    // emit) fire once per iteration rather than once per
    // outer dispatch.
    "while",
    // `dryrun` last so its short-circuit happens before any
    // inner wrapper observes the dryrun stand-in's empty
    // body. The DRYRUN registration's `forbids_outer = [every
    // other wrapper]` pins this position structurally; the
    // explicit slot here is the resolver's tiebreaker for any
    // future wrapper that DRYRUN doesn't yet forbid.
    "dryrun",
    // `fields` is intentionally outer of everything
    // including dryrun — so under `dryrun=fields` the fields
    // wrapper's pre-execute render runs BEFORE DRYRUN's
    // short-circuit (the fields render+println is the surface
    // that produces the operator-visible "what would have
    // been sent" output). Innermost-first list ordering
    // means later index = outer position at execute time.
    "fields",
    // `errors` is the ABSOLUTE OUTERMOST layer (SRD-82 Part 3b):
    // it observes the stack's one terminal outcome, routes it
    // through the op's resolved ErrorPolicy, and applies the
    // stop/fail effects. Hand-placed in the cascade (after the
    // plan loop, mirroring the hand-placed innermost retry); this
    // slot keeps the PLAN's order — telemetry, describe, override
    // validation — aligned with the runtime truth. Outside dryrun
    // / fields it is inert on their Ok short-circuits.
    "errors",
];

/// Resolves a [`WrapperPlan`] for a parsed op template.
///
/// Stateless apart from the session config it holds (the
/// default-order tiebreaker). Repeated calls with the same
/// inputs produce identical plans.
pub struct WrapperResolver {
    /// Innermost-to-outermost tiebreaker order, validated
    /// against the registry at construction time.
    default_order: Vec<WrapperName>,
}

impl WrapperResolver {
    /// Construct a resolver with the built-in default order.
    pub fn with_default_order(registry: &WrapperRegistry) -> Result<Self, ResolveError> {
        let names: Vec<&str> = DEFAULT_ORDER.to_vec();
        Self::from_names(&names, registry)
    }

    /// Construct a resolver from an explicit list of wrapper
    /// names (innermost-to-outermost). Validates against the
    /// registry's constraint graph; rejects unknown names and
    /// orderings that violate `forbids_outer`.
    pub fn from_names(names: &[&str], registry: &WrapperRegistry) -> Result<Self, ResolveError> {
        let mut order = Vec::with_capacity(names.len());
        for name in names {
            match registry.get_str(name) {
                Some(reg) => order.push(reg.name),
                None => {
                    return Err(ResolveError::UnknownWrapper {
                        name: (*name).to_string(),
                        suggestion: registry.closest_match(name),
                    });
                }
            }
        }
        // Check the default order against the WHOLE registry
        // graph: every requires_inner must be satisfied by
        // position, and no forbids_outer must be violated.
        validate_order_against_registry(&order, registry)?;
        Ok(Self {
            default_order: order,
        })
    }

    /// Resolve the wrapper plan for one op template.
    pub fn resolve(
        &self,
        subject: WrapperSubject,
        registry: &WrapperRegistry,
    ) -> Result<WrapperPlan, ResolveError> {
        // Pass 1 — trigger fan-out. Only wrappers legal at this subject's
        // level are eligible (SRD-82/92 cross-level: an op resolve sees op
        // wrappers, a phase resolve sees phase wrappers).
        let mut activations: HashMap<WrapperName, WrapperActivation> = HashMap::new();
        for reg in registry.iter() {
            if reg.applies_at(subject.level()) && (reg.triggers)(subject) {
                let activation = first_owned_field(reg, subject)
                    .map(|f| WrapperActivation::OwnedField {
                        wrapper: reg.name,
                        field: f,
                    })
                    .unwrap_or(WrapperActivation::AlwaysOn { wrapper: reg.name });
                activations.insert(reg.name, activation);
            }
        }

        // Pass 2 — transitive closure on requires_inner.
        let mut frontier: Vec<WrapperName> = activations.keys().copied().collect();
        while let Some(w) = frontier.pop() {
            let reg = registry
                .get(w)
                .expect("triggered wrapper must be registered");
            for &needed in reg.requires_inner {
                let needed_reg =
                    registry
                        .get(needed)
                        .ok_or(ResolveError::DanglingRequiresInner {
                            from: w,
                            missing: needed,
                        })?;
                let _ = needed_reg;
                if let std::collections::hash_map::Entry::Vacant(e) = activations.entry(needed) {
                    e.insert(WrapperActivation::TransitiveFrom {
                        wrapper: needed,
                        requested_by: w,
                    });
                    frontier.push(needed);
                }
            }
        }

        // Pass 3a — mutually_exclusive_with.
        for (&w, w_act) in &activations {
            let reg = registry.get(w).unwrap();
            for &peer in reg.mutually_exclusive_with {
                if let Some(peer_act) = activations.get(&peer) {
                    return Err(ResolveError::MutuallyExclusive {
                        a: w,
                        b: peer,
                        a_reason: w_act.clone(),
                        b_reason: peer_act.clone(),
                    });
                }
            }
        }

        // Pass 3b — cycle check on requires_inner over the
        // triggered set. DFS with grey/black coloring.
        if let Some(cycle) = detect_cycle(&activations, registry) {
            return Err(ResolveError::ConstraintCycle { cycle });
        }

        // Pass 4 — topological order with default-order tiebreaker.
        let stack = topo_sort(&activations, registry, &self.default_order);

        // Pass 4b — forbids_outer post-scan.
        for (i, inner) in stack.iter().enumerate() {
            for outer in &stack[i + 1..] {
                if inner.forbids_outer.contains(&outer.name) {
                    return Err(ResolveError::ForbiddenOuter {
                        inner: inner.name,
                        outer: outer.name,
                    });
                }
            }
        }

        // Build provenance vec in stack order so iteration
        // is intuitive ("first-built → last-built").
        let provenance: Vec<WrapperActivation> = stack
            .iter()
            .map(|reg| activations.get(&reg.name).cloned().unwrap())
            .collect();

        Ok(WrapperPlan { stack, provenance })
    }

    /// The innermost-to-outermost default order this resolver
    /// uses as a tiebreaker. Useful for diagnostics
    /// (`nmbrs describe wrappers`).
    pub fn default_order(&self) -> &[WrapperName] {
        &self.default_order
    }

    /// SRD-32a Push 3 — resolve a plan using an explicit
    /// per-op innermost-to-outermost order list. The list
    /// MUST be a permutation of the wrappers triggered on
    /// this op (after transitive activation):
    ///
    /// - listing a wrapper whose trigger doesn't fire is a
    ///   hard error (silently dropping it would mask typos),
    /// - omitting one whose trigger does fire is a hard
    ///   error (skipping a wrapper changes semantics).
    ///
    /// All other constraint checks (`mutually_exclusive_with`,
    /// `requires_inner` cycles, `forbids_outer`) run exactly
    /// as they do for the default-order path — same code,
    /// same error shapes.
    pub fn resolve_with_order(
        &self,
        subject: WrapperSubject,
        registry: &WrapperRegistry,
        order: &[&str],
    ) -> Result<WrapperPlan, ResolveError> {
        // Pass 1+2 — compute the triggered set the same way
        // `resolve` does. The override list is checked AGAINST
        // this set, not given the freedom to override what
        // triggers.
        let mut activations: HashMap<WrapperName, WrapperActivation> = HashMap::new();
        for reg in registry.iter() {
            if reg.applies_at(subject.level()) && (reg.triggers)(subject) {
                let activation = first_owned_field(reg, subject)
                    .map(|f| WrapperActivation::OwnedField {
                        wrapper: reg.name,
                        field: f,
                    })
                    .unwrap_or(WrapperActivation::AlwaysOn { wrapper: reg.name });
                activations.insert(reg.name, activation);
            }
        }
        let mut frontier: Vec<WrapperName> = activations.keys().copied().collect();
        while let Some(w) = frontier.pop() {
            let reg = registry
                .get(w)
                .expect("triggered wrapper must be registered");
            for &needed in reg.requires_inner {
                let _ = registry
                    .get(needed)
                    .ok_or(ResolveError::DanglingRequiresInner {
                        from: w,
                        missing: needed,
                    })?;
                if let std::collections::hash_map::Entry::Vacant(e) = activations.entry(needed) {
                    e.insert(WrapperActivation::TransitiveFrom {
                        wrapper: needed,
                        requested_by: w,
                    });
                    frontier.push(needed);
                }
            }
        }

        // Translate the override into WrapperName entries so
        // we can compare to `activations`. Unknown names get
        // a typo suggestion.
        let mut override_names: Vec<WrapperName> = Vec::with_capacity(order.len());
        for raw in order {
            match registry.get_str(raw) {
                Some(reg) => override_names.push(reg.name),
                None => {
                    return Err(ResolveError::UnknownWrapper {
                        name: (*raw).to_string(),
                        suggestion: registry.closest_match(raw),
                    });
                }
            }
        }

        // Permutation rules: every triggered wrapper must
        // appear; no wrapper appears that isn't triggered.
        let triggered: std::collections::HashSet<WrapperName> =
            activations.keys().copied().collect();
        let in_override: std::collections::HashSet<WrapperName> =
            override_names.iter().copied().collect();
        for w in &triggered {
            if !in_override.contains(w) {
                return Err(ResolveError::OverridePermutationMismatch {
                    missing: Some(*w),
                    extra: None,
                });
            }
        }
        for w in &in_override {
            if !triggered.contains(w) {
                return Err(ResolveError::OverridePermutationMismatch {
                    missing: None,
                    extra: Some(*w),
                });
            }
        }

        // Mutual-exclusion + requires_inner cycle checks
        // (same as `resolve`).
        for (&w, w_act) in &activations {
            let reg = registry.get(w).unwrap();
            for &peer in reg.mutually_exclusive_with {
                if let Some(peer_act) = activations.get(&peer) {
                    return Err(ResolveError::MutuallyExclusive {
                        a: w,
                        b: peer,
                        a_reason: w_act.clone(),
                        b_reason: peer_act.clone(),
                    });
                }
            }
        }
        if let Some(cycle) = detect_cycle(&activations, registry) {
            return Err(ResolveError::ConstraintCycle { cycle });
        }

        // The override IS the order. Validate the
        // requires_inner / forbids_outer constraints against
        // it directly: each requires_inner pair must have
        // the inner appear earlier; each forbids_outer pair
        // must have the listed wrapper appear earlier or
        // not at all.
        let pos: HashMap<WrapperName, usize> = override_names
            .iter()
            .enumerate()
            .map(|(i, &n)| (n, i))
            .collect();
        for &name in &override_names {
            let reg = registry.get(name).unwrap();
            for &needed in reg.requires_inner {
                if let (Some(&me), Some(&inner)) = (pos.get(&name), pos.get(&needed))
                    && inner >= me
                {
                    return Err(ResolveError::ForbiddenOuter {
                        inner: needed,
                        outer: name,
                    });
                }
            }
            for &forbidden in reg.forbids_outer {
                if let (Some(&me), Some(&forb)) = (pos.get(&name), pos.get(&forbidden))
                    && forb > me
                {
                    return Err(ResolveError::ForbiddenOuter {
                        inner: name,
                        outer: forbidden,
                    });
                }
            }
        }

        let stack: Vec<&'static WrapperRegistration> = override_names
            .iter()
            .map(|n| registry.get(*n).unwrap())
            .collect();
        let provenance: Vec<WrapperActivation> = stack
            .iter()
            .map(|reg| activations.get(&reg.name).cloned().unwrap())
            .collect();
        Ok(WrapperPlan { stack, provenance })
    }
}

/// First owned field present on the template, used to label
/// `OwnedField` activations. Returns `None` when the wrapper
/// has no owned fields (e.g. `traverse`, `result`); the caller
/// falls back to `AlwaysOn`.
fn first_owned_field(
    reg: &'static WrapperRegistration,
    subject: WrapperSubject,
) -> Option<&'static str> {
    for field in reg.owned_fields {
        if subject.has_owned_field(field) {
            return Some(*field);
        }
    }
    None
}

fn detect_cycle(
    activations: &HashMap<WrapperName, WrapperActivation>,
    registry: &WrapperRegistry,
) -> Option<Vec<WrapperName>> {
    #[derive(Copy, Clone, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }

    let mut color: HashMap<WrapperName, Color> =
        activations.keys().map(|&n| (n, Color::White)).collect();
    let mut stack: Vec<WrapperName> = Vec::new();

    fn dfs(
        node: WrapperName,
        color: &mut HashMap<WrapperName, Color>,
        stack: &mut Vec<WrapperName>,
        registry: &WrapperRegistry,
        activations: &HashMap<WrapperName, WrapperActivation>,
    ) -> Option<Vec<WrapperName>> {
        color.insert(node, Color::Grey);
        stack.push(node);
        let reg = registry.get(node).unwrap();
        for &needed in reg.requires_inner {
            if !activations.contains_key(&needed) {
                continue;
            }
            match color.get(&needed).copied().unwrap_or(Color::White) {
                Color::Grey => {
                    // cycle — slice from `needed` to top of stack
                    let cycle_start = stack.iter().position(|&n| n == needed).unwrap();
                    let mut cycle = stack[cycle_start..].to_vec();
                    cycle.push(needed);
                    return Some(cycle);
                }
                Color::White => {
                    if let Some(c) = dfs(needed, color, stack, registry, activations) {
                        return Some(c);
                    }
                }
                Color::Black => {}
            }
        }
        stack.pop();
        color.insert(node, Color::Black);
        None
    }

    let nodes: Vec<WrapperName> = activations.keys().copied().collect();
    for n in nodes {
        if color.get(&n).copied() == Some(Color::White)
            && let Some(c) = dfs(n, &mut color, &mut stack, registry, activations)
        {
            return Some(c);
        }
    }
    None
}

/// Topological sort of triggered wrappers honouring
/// `requires_inner` (inner before outer). When the partial
/// order leaves choices, the session-level default order
/// breaks ties.
///
/// Uses Kahn's algorithm with a stable selection rule:
/// among nodes with no remaining inner-edges, pick the one
/// that appears earliest in `default_order`. Wrappers not
/// listed in default_order sort last alphabetically.
fn topo_sort(
    activations: &HashMap<WrapperName, WrapperActivation>,
    registry: &WrapperRegistry,
    default_order: &[WrapperName],
) -> Vec<&'static WrapperRegistration> {
    // Build "inner_count" — number of triggered wrappers in
    // this wrapper's requires_inner that haven't been emitted
    // yet. When zero, the wrapper is eligible.
    let mut inner_count: HashMap<WrapperName, usize> = HashMap::new();
    for &w in activations.keys() {
        let reg = registry.get(w).unwrap();
        let count = reg
            .requires_inner
            .iter()
            .filter(|n| activations.contains_key(*n))
            .count();
        inner_count.insert(w, count);
    }

    // Reverse adjacency: for each wrapper W, which triggered
    // wrappers list W in their requires_inner?
    let mut requires_me: HashMap<WrapperName, Vec<WrapperName>> = HashMap::new();
    for &w in activations.keys() {
        let reg = registry.get(w).unwrap();
        for &needed in reg.requires_inner {
            if activations.contains_key(&needed) {
                requires_me.entry(needed).or_default().push(w);
            }
        }
    }

    let order_index: HashMap<WrapperName, usize> = default_order
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, i))
        .collect();
    let tiebreak = |a: &WrapperName, b: &WrapperName| {
        let ai = order_index.get(a).copied().unwrap_or(usize::MAX);
        let bi = order_index.get(b).copied().unwrap_or(usize::MAX);
        ai.cmp(&bi).then_with(|| a.0.cmp(b.0))
    };

    let mut emitted = HashSet::new();
    let mut out: Vec<&'static WrapperRegistration> = Vec::with_capacity(activations.len());
    while emitted.len() < activations.len() {
        let mut eligible: Vec<WrapperName> = activations
            .keys()
            .copied()
            .filter(|w| !emitted.contains(w) && inner_count[w] == 0)
            .collect();
        eligible.sort_by(|a, b| tiebreak(a, b));
        let next = eligible
            .first()
            .copied()
            .expect("cycle detection should have caught a graph with no eligible node");
        emitted.insert(next);
        out.push(registry.get(next).unwrap());
        if let Some(consumers) = requires_me.get(&next) {
            for &c in consumers {
                if let Some(slot) = inner_count.get_mut(&c) {
                    *slot -= 1;
                }
            }
        }
    }
    out
}

/// Validate an explicit innermost-to-outermost order
/// against the registry's constraint graph. Used by
/// [`WrapperResolver::from_names`] at startup so a
/// misconfigured `--wrap-default-order` fails fast.
fn validate_order_against_registry(
    order: &[WrapperName],
    registry: &WrapperRegistry,
) -> Result<(), ResolveError> {
    let pos: HashMap<WrapperName, usize> = order.iter().enumerate().map(|(i, &n)| (n, i)).collect();

    for &name in order {
        let reg = registry
            .get(name)
            .expect("name was looked up by from_names");

        // requires_inner: every named inner must appear
        // earlier (lower index) in the order — but only
        // when both the requirer and the required are in
        // the configured order. (A default-order list may
        // omit wrappers that always get pulled in
        // transitively.)
        for &needed in reg.requires_inner {
            if let (Some(&me), Some(&inner)) = (pos.get(&name), pos.get(&needed))
                && inner >= me
            {
                return Err(ResolveError::ForbiddenOuter {
                    inner: needed,
                    outer: name,
                });
            }
        }

        // forbids_outer: every named outer must appear
        // earlier (lower index) — i.e. not outside this
        // wrapper.
        for &forbidden in reg.forbids_outer {
            if let (Some(&me), Some(&forb)) = (pos.get(&name), pos.get(&forbidden))
                && forb > me
            {
                return Err(ResolveError::ForbiddenOuter {
                    inner: name,
                    outer: forbidden,
                });
            }
        }
    }
    Ok(())
}

// Tests in this module are unit-level against synthetic
// registries — they don't rely on the production
// wrapper registrations. Integration tests covering the
// production registry sit in `wrappers.rs` alongside the
// existing wrapper tests.
