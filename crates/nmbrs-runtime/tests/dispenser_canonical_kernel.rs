// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Every leaf dispenser must return the op-template kernel it was mapped
//! against from `canonical_kernel()`.
//!
//! The executor materialises one per-fiber kernel per dispenser FROM that
//! kernel's program, and the wrapper-side `PullPlan` is built against the very
//! same op-template program (`OpBuilder::program_for_op`). A leaf that returns
//! `None` leaves the per-op slot empty, so the plan falls back to the PHASE
//! kernel — and a plan holds resolved indices, not names.
//!
//! The consequences are both bad and asymmetric. Where the stale index happens
//! to be in range, the read silently returns a neighbouring wire — one metric
//! reported another's value under its own name. Where it is out of range, the
//! engine panics with "index out of bounds", naming nothing that points back
//! at the cause. The scylla CQL dispensers had exactly this defect while the
//! cassandra-cpp ones did not, so it reproduced only under `cqldriver=scylla`.
//!
//! This test pins the contract at the source level, which is where it is
//! cheap and total: a leaf `impl OpDispenser` that never mentions
//! `canonical_kernel` inherits the delegating default and returns `None`.

use std::path::Path;

/// Leaf dispensers — those with no `inner_dispenser` to delegate to — must
/// implement `canonical_kernel`.
#[test]
fn every_leaf_dispenser_returns_its_op_template_kernel() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("nmbrs-runtime lives in crates/")
        .parent()
        .expect("workspace root");
    let mut offenders: Vec<String> = Vec::new();

    // Every adapter is a crate at `crates/nmbrs-adapter-<name>`.
    let adapter_dirs: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("crates"))
        .expect("crates/ is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("nmbrs-adapter-"))
        })
        .collect();
    assert!(
        !adapter_dirs.is_empty(),
        "no crates/nmbrs-adapter-* directories found under {root:?}"
    );

    for entry in adapter_dirs.iter().flat_map(|d| walk(d)) {
        let src = std::fs::read_to_string(&entry).unwrap_or_default();
        if !src.contains("impl OpDispenser for") && !src.contains("OpDispenser for") {
            continue;
        }
        // A wrapper delegates; a leaf does not. Only leaves must declare it.
        let is_leaf = !src.contains("fn inner_dispenser");
        if is_leaf && !src.contains("fn canonical_kernel") {
            offenders.push(
                entry
                    .strip_prefix(root)
                    .unwrap_or(&entry)
                    .display()
                    .to_string(),
            );
        }
    }

    assert!(
        offenders.is_empty(),
        "these leaf dispensers do not implement `canonical_kernel()`, so their \
         per-op kernel slot stays empty and every wrapper PullPlan built \
         against the op-template program silently reindexes against the phase \
         kernel:\n  {}",
        offenders.join("\n  ")
    );
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}
