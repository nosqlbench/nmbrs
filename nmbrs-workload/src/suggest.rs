// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Deep workload suggestions — the shared "did you mean" set.
//!
//! When an exact workload reference misses, three callers want the same
//! answer: the shell-completion provider (`nmbrs::completion`) as its
//! fallback, and the resolver's not-found paths
//! (`nmbrs_runtime::runner::resolve_workload`, [`crate::verify::verify_target`],
//! `nmbrs::copy_cmd`) as the suggestion tail on the error. Each asks the
//! same question — *every workload whose final name segment begins with
//! what the operator typed* — over the local file hierarchy and the
//! bundled catalog.
//!
//! Leaf-segment matching is the point: it is what makes a bare
//! `phase_poll` surface a buried `examples/controls/phase_poll_smoke`. The
//! catalog carries the namespace; the operator types the stem. A
//! slash-qualified partial (`examples/controls/phase_poll`) still matches
//! by full-reference prefix, so both habits work.
//!
//! The local walk descends [`MAX_DEPTH`] directory levels below the start
//! directory (the operator's cwd); the bundled catalog is a flat namespace
//! with no depth bound. SRD-85 makes the catalog the discovery surface, so
//! a suggestion drawn from it is always runnable by the name shown.

use std::path::Path;

/// Directory levels the local walk descends below the start directory. A
/// file at `a/b/c/<name>.yaml` is the deepest reached at the default of 3.
/// The bundled catalog is flat and not subject to this bound.
pub const MAX_DEPTH: usize = 3;

/// Cap on directory entries the local walk reads in one call, so a
/// suggestion never degrades into an unbounded tree crawl in a large repo.
const MAX_ENTRIES_SCANNED: usize = 4000;

/// Most names to spell out in a "did you mean" tail before collapsing the
/// remainder into a `(+N more …)` note.
const MAX_LISTED: usize = 10;

/// Directories never worth walking for workloads.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "logs", ".git"];

/// Every workload reference matching `partial` by leaf segment (or by
/// full-reference prefix), drawn from the local file hierarchy below the
/// current directory (down to [`MAX_DEPTH`] levels) **and** the bundled
/// catalog. Sorted and deduped.
///
/// The completion provider drops to this when no prefix candidate is
/// obvious near the cursor; the `run`/`check` resolvers feed it to
/// [`did_you_mean`]. Empty `partial` yields nothing — there is no leaf to
/// match on, and offering the whole tree as a suggestion helps no one.
pub fn suggest_workloads(partial: &str) -> Vec<String> {
    if partial.is_empty() {
        return Vec::new();
    }
    let needle = leaf_of(partial);
    let mut out = Vec::new();
    let mut budget = MAX_ENTRIES_SCANNED;
    walk(
        Path::new("."),
        String::new(),
        partial,
        needle,
        0,
        &mut budget,
        &mut out,
    );
    out.extend(bundled_matches(partial, needle));
    out.sort();
    out.dedup();
    out
}

/// Catalog-only matches, for callers that run bundled names exclusively
/// (`nmbrs copy`): suggesting a local file there would name something the
/// command cannot act on.
pub fn suggest_bundled(partial: &str) -> Vec<String> {
    if partial.is_empty() {
        return Vec::new();
    }
    let mut out = bundled_matches(partial, leaf_of(partial));
    out.sort();
    out.dedup();
    out
}

/// Render a hit list (from [`suggest_workloads`] / [`suggest_bundled`]) as
/// a human "did you mean" tail, ready to append to an error message:
/// ` Did you mean: a, b?` for a short list, or
/// ` Did you mean one of: a, b (+N more — \`nmbrs describe workloads --all\`)`
/// when capped at [`MAX_LISTED`]. Empty string when `hits` is empty, so
/// callers can append it unconditionally.
pub fn did_you_mean(hits: &[String]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = hits.iter().take(MAX_LISTED).map(String::as_str).collect();
    let overflow = hits.len() - shown.len();
    let list = shown.join(", ");
    if overflow > 0 {
        format!(
            " Did you mean one of: {list} \
             (+{overflow} more — `nmbrs describe workloads --all`)"
        )
    } else {
        format!(" Did you mean: {list}?")
    }
}

/// Bundled-catalog entries whose full name prefixes `partial` or whose
/// final segment prefixes `needle`.
fn bundled_matches(partial: &str, needle: &str) -> Vec<String> {
    crate::catalog::iter()
        .map(|w| w.name)
        .filter(|n| n.starts_with(partial) || leaf_of(n).starts_with(needle))
        .map(str::to_string)
        .collect()
}

/// The final `/`-separated segment of `s` (the whole string if no `/`).
fn leaf_of(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// True when a local file path (relative to cwd) is already represented in
/// the bundled catalog: it lives under a bundle-source root and its mapped
/// catalog name resolves. Mirrors `nmbrs/build.rs`'s SRD-85 naming, so the
/// repo's own example files don't double up with their embedded copies in a
/// suggestion list. The double-up is what makes `examples/dy<TAB>` collapse
/// to the shared `examples/` prefix (catalog `examples/controls/…` vs file
/// `nmbrs/examples/workloads/controls/….yaml`); dropping the redundant file
/// leaves one clean candidate bash can complete to.
fn is_catalog_duplicate(rel: &str) -> bool {
    catalog_name_for_local(rel).is_some_and(|n| crate::catalog::lookup(&n).is_some())
}

/// The catalog name a local file under a bundle-source root would carry, or
/// `None` if it isn't under one. Inverse of `nmbrs/build.rs`:
/// `workloads/<x>` → `<x>` (so `workloads/cql/<x>` → `cql/<x>`) and
/// `examples/workloads/<x>` → `examples/<x>` (extension stripped) — the
/// SRD-85 logical layout relative to the cwd.
fn catalog_name_for_local(rel: &str) -> Option<String> {
    let stem = rel
        .strip_suffix(".yaml")
        .or_else(|| rel.strip_suffix(".yml"))?;
    if let Some(rest) = stem.strip_prefix("examples/workloads/") {
        return Some(format!("examples/{rest}"));
    }
    stem.strip_prefix("workloads/").map(str::to_string)
}

/// Recursive yaml walk. Descends every directory (skipping hidden and
/// [`SKIP_DIRS`]) until `depth` reaches [`MAX_DEPTH`], emitting each yaml
/// file whose relative path prefixes `partial` or whose stem prefixes
/// `needle`. Bounded by `budget` entries read.
fn walk(
    dir: &Path,
    rel: String,
    partial: &str,
    needle: &str,
    depth: usize,
    budget: &mut usize,
    out: &mut Vec<String>,
) {
    if *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            if depth < MAX_DEPTH {
                walk(&path, child_rel, partial, needle, depth + 1, budget, out);
            }
            continue;
        }
        let Some(stem) = name
            .strip_suffix(".yaml")
            .or_else(|| name.strip_suffix(".yml"))
        else {
            continue;
        };
        if (child_rel.starts_with(partial) || stem.starts_with(needle))
            && !is_catalog_duplicate(&child_rel)
        {
            out.push(child_rel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, "ops: { a: { raw: x } }\n").unwrap();
    }

    /// Walk a freshly-built tree directly (the public entry point keys off
    /// the process cwd, which a test must not mutate globally).
    fn local_hits(root: &Path, partial: &str) -> Vec<String> {
        let needle = leaf_of(partial);
        let mut out = Vec::new();
        let mut budget = MAX_ENTRIES_SCANNED;
        walk(
            root,
            String::new(),
            partial,
            needle,
            0,
            &mut budget,
            &mut out,
        );
        out.sort();
        out
    }

    #[test]
    fn leaf_match_surfaces_buried_stem() {
        let dir = std::env::temp_dir().join(format!("nmbrs-suggest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        touch(&dir.join("examples/workloads/controls/phase_poll_smoke.yaml"));
        let hits = local_hits(&dir, "phase_poll");
        assert_eq!(
            hits,
            vec!["examples/workloads/controls/phase_poll_smoke.yaml"]
        );
    }

    #[test]
    fn depth_bound_excludes_level_four() {
        let dir = std::env::temp_dir().join(format!("nmbrs-suggest-depth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // a/b/c/<file> is the deepest reached (3 dir levels); a/b/c/d/<file> is not.
        touch(&dir.join("a/b/c/match_me.yaml"));
        touch(&dir.join("a/b/c/d/match_me.yaml"));
        let hits = local_hits(&dir, "match_me");
        assert_eq!(hits, vec!["a/b/c/match_me.yaml"]);
    }

    #[test]
    fn slash_qualified_partial_matches_by_full_prefix() {
        let dir = std::env::temp_dir().join(format!("nmbrs-suggest-slash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        touch(&dir.join("cursors/all_cursor/enumerate.yaml"));
        let hits = local_hits(&dir, "cursors/all_cursor/enum");
        assert_eq!(hits, vec!["cursors/all_cursor/enumerate.yaml"]);
    }

    #[test]
    fn skip_dirs_are_not_walked() {
        let dir = std::env::temp_dir().join(format!("nmbrs-suggest-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        touch(&dir.join("target/buried/match_me.yaml"));
        touch(&dir.join("kept/match_me.yaml"));
        let hits = local_hits(&dir, "match_me");
        assert_eq!(hits, vec!["kept/match_me.yaml"]);
    }

    #[test]
    fn local_path_maps_to_catalog_name() {
        // Inverse of build.rs's SRD-85 naming — the equivalence used to
        // dedup a repo example file against its embedded catalog twin.
        assert_eq!(
            catalog_name_for_local("examples/workloads/controls/phase_poll_smoke.yaml").as_deref(),
            Some("examples/controls/phase_poll_smoke")
        );
        assert_eq!(
            catalog_name_for_local("workloads/keyvalue.yaml").as_deref(),
            Some("keyvalue")
        );
        assert_eq!(
            catalog_name_for_local("workloads/cql/baselinesv3/keyvalue.yml").as_deref(),
            Some("cql/baselinesv3/keyvalue")
        );
        // A file outside any bundle-source root has no catalog twin.
        assert_eq!(catalog_name_for_local("my/own/workload.yaml"), None);
        assert_eq!(catalog_name_for_local("examples/notes/readme.txt"), None);
    }

    #[test]
    fn empty_partial_yields_nothing() {
        assert!(suggest_workloads("").is_empty());
        assert!(suggest_bundled("").is_empty());
    }

    #[test]
    fn did_you_mean_formats_short_and_capped_lists() {
        assert_eq!(did_you_mean(&[]), "");
        assert_eq!(
            did_you_mean(&["a".to_string(), "b".to_string()]),
            " Did you mean: a, b?"
        );
        let many: Vec<String> = (0..MAX_LISTED + 3).map(|i| format!("w{i}")).collect();
        let tail = did_you_mean(&many);
        assert!(tail.contains("+3 more"), "overflow note: {tail}");
        assert!(
            tail.contains("Did you mean one of:"),
            "capped phrasing: {tail}"
        );
    }
}
