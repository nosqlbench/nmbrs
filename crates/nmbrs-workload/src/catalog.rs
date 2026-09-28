// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Bundled-workload catalog — SRD-85.
//!
//! Workloads embedded into the artifact, discoverable by
//! catalog name (`cql/keyvalue`, `examples/lfsr`). The catalog
//! is assembled once at process startup from the manifests each
//! contributing crate generates at build time (see
//! `tools/bundle-gen`): the core binary contributes the curated
//! `workloads/` set and the `examples/` tier; adapter crates
//! contribute their own `adapters/<a>/workloads/` sets behind
//! their feature gates, so the catalog is truthful about what
//! *this* binary can run.
//!
//! Visibility tiers separate the two audiences: everything in
//! the catalog is runnable by name, but only the
//! [`Tier::Curated`] entries are *listed* by default —
//! `nmbrs describe workloads` shows the products, not fifty test
//! fixtures; `--all` (or the `examples` subtopic) reveals the
//! rest.
//!
//! Resolution policy lives with the resolver
//! (`nmbrs-runtime::runner::resolve_workload`): nearest-first —
//! the logical filesystem location is favored over the catalog
//! by default, and a name that resolves both ways picks the
//! local file with a LOGGED WARNING naming both candidates.
//! Shadowing is allowed but never silent; `--strict` promotes
//! the warning to an error.

use std::sync::OnceLock;

/// Visibility tier of a bundled workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Curated, real-world workloads — listed by default.
    /// Required to carry a `description:` (lint-enforced).
    Curated,
    /// Teaching examples and coverage workloads — bundled and
    /// runnable (artifact smoke-testing as-is, anywhere), but
    /// unlisted unless explicitly requested.
    Example,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Curated => "curated",
            Tier::Example => "example",
        }
    }
}

/// One embedded workload.
#[derive(Debug)]
pub struct BundledWorkload {
    /// Catalog name: `<namespace>/<stem>` (no extension), e.g.
    /// `cql/keyvalue`, `examples/signals/lfsr`. Top-level
    /// curated entries have no namespace.
    pub name: &'static str,
    /// Visibility tier.
    pub tier: Tier,
    /// The yaml source, embedded at build time.
    pub source: &'static str,
}

static CATALOG: OnceLock<Vec<&'static BundledWorkload>> = OnceLock::new();

/// Install the catalog from the contributing manifests. Called
/// once at process startup (before any workload resolution);
/// subsequent calls are ignored — the first installation wins,
/// which keeps test harnesses that re-enter startup idempotent.
///
/// Panics on a duplicate catalog name across sets: two crates
/// claiming one name is a build/packaging bug, not an operator
/// condition.
pub fn install(sets: &[&'static [BundledWorkload]]) {
    let _ = CATALOG.set({
        let mut all: Vec<&'static BundledWorkload> = sets.iter().flat_map(|s| s.iter()).collect();
        all.sort_by_key(|w| w.name);
        for pair in all.windows(2) {
            assert_ne!(
                pair[0].name, pair[1].name,
                "bundled workload name collision across catalog sets: `{}`",
                pair[0].name,
            );
        }
        all
    });
}

/// Exact-name lookup. No globbing, no fuzzy matching —
/// `nmbrs describe workloads` is the discovery surface, not the
/// resolver.
pub fn lookup(name: &str) -> Option<&'static BundledWorkload> {
    CATALOG
        .get()?
        .binary_search_by_key(&name, |w| w.name)
        .ok()
        .map(|idx| CATALOG.get().unwrap()[idx])
}

/// All catalog entries in name order. Empty when no catalog was
/// installed (library consumers without the nmbrs binary's
/// startup hook).
pub fn iter() -> impl Iterator<Item = &'static BundledWorkload> {
    CATALOG
        .get()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
        .iter()
        .copied()
}

/// Entries of one tier, in name order.
pub fn iter_tier(tier: Tier) -> impl Iterator<Item = &'static BundledWorkload> {
    iter().filter(move |w| w.tier == tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The install-once global makes classic unit isolation
    // awkward; these tests exercise the pure parts and a single
    // install path. (E2e coverage drives the real assembled
    // catalog through the nmbrs binary.)
    static SET_A: &[BundledWorkload] = &[
        BundledWorkload {
            name: "alpha",
            tier: Tier::Curated,
            source: "description: a\n",
        },
        BundledWorkload {
            name: "examples/beta",
            tier: Tier::Example,
            source: "# b\n",
        },
    ];

    #[test]
    fn install_lookup_and_tier_filter() {
        install(&[SET_A]);
        // Idempotent re-install is a no-op.
        install(&[]);
        assert!(lookup("alpha").is_some());
        assert!(lookup("examples/beta").is_some());
        assert!(lookup("nope").is_none());
        let curated: Vec<_> = iter_tier(Tier::Curated).map(|w| w.name).collect();
        assert_eq!(curated, vec!["alpha"]);
        let all: Vec<_> = iter().map(|w| w.name).collect();
        assert_eq!(all, vec!["alpha", "examples/beta"]);
    }
}
