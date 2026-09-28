// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Dimensional cells materialised from data.
//!
//! A metric whose series are one-per-instance of some dimension — a compaction
//! tier, a keyspace, a node — does NOT get there by attaching labels to an
//! instrument. The label set of a [`Component`] *is* the dimensional cell:
//!
//! > The component's `effective_labels` define the dimensional cell; the same
//! > family on a different component is a different cell and produces no
//! > collision.
//!
//! So one series per instance is one CHILD COMPONENT per instance, with the
//! family registered exactly once on each. The duplicate-family rejection in
//! [`Component::register_instrument`] is untouched and keeps catching what it
//! exists to catch: two different instruments claiming one name inside one
//! dimensional context.
//!
//! This registry is the resolve-or-create for those children. It is
//! deliberately generic — it knows about coordinates and components, not about
//! metrics — and it hangs off the component tree, so a cell's lifetime is its
//! parent's: cells created under a phase component die when that phase's
//! subtree is dropped.
//!
//! Coordinates are whole. A two-dimension coordinate resolves to ONE child
//! carrying both labels, never to nested children — nesting would impose an
//! arbitrary order on co-equal dimensions, and `a` inside `b` is a different
//! tree shape from `b` inside `a` for no reason the data supports.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::component::Component;
use crate::labels::Labels;

/// Resolve-or-create map from a coordinate to the child component that
/// represents it. Interior-mutable so it is reachable through a read guard on
/// the parent, mirroring [`Component::controls`].
#[derive(Default)]
pub struct CellMap {
    /// Keyed by the coordinate's canonical rendering. `Labels::to_prometheus`
    /// is order-stable, so two spellings of one coordinate map to one cell.
    inner: Mutex<HashMap<String, Arc<RwLock<Component>>>>,
}

impl CellMap {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// The cell for `coord` under `parent`, creating and attaching it on first
    /// sight. Idempotent: the same coordinate yields the same component for as
    /// long as the parent lives, so a caller may resolve per cycle and get a
    /// registry write only once.
    ///
    /// `coord` carries only the dimension labels this cell adds. Inherited
    /// labels come from the parent via [`crate::component::attach`], which also
    /// enforces that no ancestor already owns these names.
    pub fn resolve(
        &self,
        parent: &Arc<RwLock<Component>>,
        coord: &Labels,
    ) -> Arc<RwLock<Component>> {
        let key = coord.to_prometheus();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(&key) {
            return existing.clone();
        }
        let mut component = Component::new(coord.clone(), HashMap::new());
        // Running from birth. `Component::new` starts in `Starting`, and the
        // cadence walk captures only `Running` components — a cell left
        // `Starting` would accept samples and emit none of them. A cell exists
        // because data is already flowing into it, so there is no window in
        // which `Starting` would be the honest state.
        component.set_state(crate::component::ComponentState::Running);
        let child = Arc::new(RwLock::new(component));
        crate::component::attach(parent, &child);
        map.insert(key, child.clone());
        child
    }

    /// Number of distinct coordinates materialised so far. For diagnostics and
    /// tests — there is deliberately no cap: how many series a dimension has is
    /// a modelling decision, and silently dropping data would be worse than
    /// having many.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Resolve the cell for `coord` under `parent`, creating it on first sight.
///
/// Free function rather than a method so the lock discipline lives in ONE
/// place: resolving attaches a child, which takes `parent`'s write lock, so a
/// caller holding a read guard across the call self-deadlocks on the same
/// `RwLock`. Binding the `Arc` first is what releases it.
///
/// `parent` must be **the component the metric registers on**, not an ambient
/// one. A cell REFINES an identity: the coordinate adds dimensions to the label
/// set that component already contributes. Sourcing the parent from ambient
/// context instead composes identity from wherever the code happens to be
/// running, which silently drops the parts the registration site owns (`op=`,
/// most obviously) — a different metric identity wearing the same family name.
pub fn resolve_under(parent: &Arc<RwLock<Component>>, coord: &Labels) -> Arc<RwLock<Component>> {
    let cells = parent.read().unwrap_or_else(|e| e.into_inner()).cells();
    cells.resolve(parent, coord)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> Arc<RwLock<Component>> {
        Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "finalize"),
            HashMap::new(),
        )))
    }

    /// The same coordinate must resolve to the SAME component, or a per-cycle
    /// resolve would attach a new child every cycle and re-register the family
    /// on each — the registry write this design exists to avoid.
    #[test]
    fn a_coordinate_resolves_to_one_stable_cell() {
        let p = parent();
        let cells = CellMap::new();
        let a = cells.resolve(&p, &Labels::of("tier", "24"));
        let b = cells.resolve(&p, &Labels::of("tier", "24"));
        assert!(Arc::ptr_eq(&a, &b), "one coordinate must be one cell");
        assert_eq!(cells.len(), 1);
    }

    /// Distinct values are distinct cells — which is what makes one family
    /// registrable on each without colliding.
    #[test]
    fn distinct_values_are_distinct_cells() {
        let p = parent();
        let cells = CellMap::new();
        let a = cells.resolve(&p, &Labels::of("tier", "24"));
        let b = cells.resolve(&p, &Labels::of("tier", "25"));
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(cells.len(), 2);

        // The registry invariant this whole design protects: the same family
        // registers cleanly on both, because they are different cells.
        for c in [&a, &b] {
            let g = c.write().unwrap();
            let mut g = g;
            g.register_instrument(
                "compaction_bytes_out",
                crate::component::InstrumentRef::Gauge(Arc::new(
                    crate::instruments::gauge::ValueGauge::new(Labels::default()),
                )),
            )
            .expect("same family on a different cell must not collide");
        }
    }

    /// A cell inherits its parent's labels and adds its own, so the emitted
    /// series carries the full coordinate.
    #[test]
    fn a_cell_inherits_the_parent_dimensions() {
        let p = parent();
        let cells = CellMap::new();
        let c = cells.resolve(&p, &Labels::of("tier", "24"));
        let eff = c.read().unwrap().effective_labels().to_prometheus();
        assert!(
            eff.contains("phase=") && eff.contains("tier="),
            "cell must carry inherited + own dimensions, got {eff}"
        );
    }

    /// Two dimensions are ONE cell carrying both, not a nesting.
    #[test]
    fn a_multi_dimension_coordinate_is_a_single_child() {
        let p = parent();
        let cells = CellMap::new();
        let coord = Labels::of("tier", "24").with("keyspace", "baselines");
        let c = cells.resolve(&p, &coord);
        assert_eq!(
            c.read().unwrap().child_count(),
            0,
            "a coordinate must not nest one dimension inside another"
        );
        let eff = c.read().unwrap().effective_labels().to_prometheus();
        assert!(
            eff.contains("tier=") && eff.contains("keyspace="),
            "both dimensions belong to one cell, got {eff}"
        );
        assert_eq!(p.read().unwrap().child_count(), 1);
    }

    /// Identity is the LABEL SET (with the family name promoted into it), so a
    /// cell must add to the parent's dimensions, never stand in for them. If a
    /// cell were parented somewhere else, the emitted identity would silently
    /// lose whatever the registration site owned.
    #[test]
    fn a_cell_refines_the_parents_identity_rather_than_replacing_it() {
        let phase = parent();
        let op = Arc::new(RwLock::new(Component::new(
            Labels::of("op", "read_history"),
            HashMap::new(),
        )));
        crate::component::attach(&phase, &op);

        let cell = resolve_under(&op, &Labels::of("tier", "24"));
        let eff = cell.read().unwrap().effective_labels().to_prometheus();

        for owned in ["phase=", "op=", "tier="] {
            assert!(
                eff.contains(owned),
                "a cell must carry every dimension its ancestors own; {owned} \
                 missing from {eff}"
            );
        }
    }

    /// Two siblings sharing a label set AT THE SAME TIME is the case that
    /// breaks identity: each can register the same family, and the two
    /// instruments then wear one identity with the per-component duplicate
    /// check unable to see it.
    #[test]
    #[should_panic(expected = "sibling-identity violation")]
    fn concurrent_siblings_cannot_share_a_label_set() {
        let p = parent();
        let _first = {
            let c = Arc::new(RwLock::new(Component::new(
                Labels::of("tier", "24"),
                HashMap::new(),
            )));
            crate::component::attach(&p, &c);
            c // kept alive and never stopped
        };
        let second = Arc::new(RwLock::new(Component::new(
            Labels::of("tier", "24"),
            HashMap::new(),
        )));
        crate::component::attach(&p, &second);
    }

    /// Sequential reuse must stay legal — an iteration whose values repeat
    /// (fib yields `n=1` twice) re-materialises the SAME identity, which is one
    /// identity sampled again over time. An unconditional check panicked here.
    #[test]
    fn a_label_set_may_be_reused_after_the_previous_component_stops() {
        use crate::component::ComponentState;
        let p = parent();
        let first = Arc::new(RwLock::new(Component::new(
            Labels::of("tier", "24"),
            HashMap::new(),
        )));
        crate::component::attach(&p, &first);
        first.write().unwrap().set_state(ComponentState::Stopped);

        let second = Arc::new(RwLock::new(Component::new(
            Labels::of("tier", "24"),
            HashMap::new(),
        )));
        crate::component::attach(&p, &second); // must not panic
        assert_eq!(p.read().unwrap().child_count(), 2);
    }

    /// The index must not keep a dead component alive, and must not leak a
    /// phantom claim when a component is dropped without being stopped.
    #[test]
    fn a_dropped_component_releases_its_claim() {
        let p = parent();
        {
            let c = Arc::new(RwLock::new(Component::new(
                Labels::of("tier", "24"),
                HashMap::new(),
            )));
            crate::component::attach(&p, &c);
            // `children` holds a strong ref, so drop that too: this models a
            // component detached and released rather than stopped.
            crate::component::detach(&p, &c);
        }
        let again = Arc::new(RwLock::new(Component::new(
            Labels::of("tier", "24"),
            HashMap::new(),
        )));
        crate::component::attach(&p, &again); // must not panic
    }

    /// Distinct values attach as distinct siblings — the thing cells exist to
    /// create.
    #[test]
    fn distinct_siblings_still_attach() {
        let p = parent();
        for v in ["24", "25", "26"] {
            let c = Arc::new(RwLock::new(Component::new(
                Labels::of("tier", v),
                HashMap::new(),
            )));
            crate::component::attach(&p, &c);
        }
        assert_eq!(p.read().unwrap().child_count(), 3);
    }

    /// So the guarantee has to come from HERE: the same coordinate resolves to
    /// the existing cell instead of attaching a twin, which is what keeps one
    /// coordinate mapped to one identity.
    #[test]
    fn the_resolver_is_what_prevents_a_duplicated_cell_identity() {
        let p = parent();
        let coord = Labels::of("tier", "24");
        for _ in 0..5 {
            resolve_under(&p, &coord);
        }
        assert_eq!(
            p.read().unwrap().child_count(),
            1,
            "repeated resolution must not multiply cells for one coordinate"
        );
    }

    /// Cells route through the same check, so a resolver cannot be the only
    /// thing standing between the tree and a duplicated identity.
    #[test]
    fn repeated_cell_resolution_does_not_trip_the_sibling_check() {
        let p = parent();
        let coord = Labels::of("tier", "24");
        let first = resolve_under(&p, &coord);
        let again = resolve_under(&p, &coord);
        assert!(
            Arc::ptr_eq(&first, &again),
            "memoisation must return the existing cell rather than attach a twin"
        );
        assert_eq!(p.read().unwrap().child_count(), 1);
    }
}
