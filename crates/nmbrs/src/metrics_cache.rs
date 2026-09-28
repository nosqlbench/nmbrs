// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Dimensional-label cache for `nmbrs metrics match` tab
//! completion. Persisted alongside `metrics.db` as a JSON
//! sidecar; mtime synced to the db's mtime so a single
//! mtime-comparison answers "is the cache still fresh?".
//!
//! Cache structure:
//!
//! ```text
//! { "families": ["recall@10.mean", ...],
//!   "labels":   { "k": ["1","10","100"], "profile": [...], ... } }
//! ```
//!
//! Refresh policy: build on first read, persist with
//! `cache.mtime = db.mtime`. On subsequent reads, if
//! `cache.mtime < db.mtime` rebuild. A db that hasn't changed
//! since the cache was built keeps its previous mtime, and
//! the comparison short-circuits the rebuild.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MetricsCache {
    /// Distinct metric family names (the prefix before `{` in
    /// each `metric_instance.spec`). Sorted alphabetically.
    pub families: Vec<String>,
    /// label_key → sorted distinct values seen on any
    /// instance.
    pub labels: BTreeMap<String, Vec<String>>,
}

impl MetricsCache {
    /// Load the cache for `db_path`, rebuilding from the
    /// metric_instance table when the cache file is missing,
    /// unparseable, or older than the db. Cache lives next to
    /// the db. Failures bubble up as an empty cache so
    /// completion stays best-effort and never blocks tab.
    pub fn load_or_build(db_path: &Path) -> Self {
        let cache_path = derive_cache_path(db_path);
        if let Some(c) = load_if_fresh(&cache_path, db_path) {
            return c;
        }
        let built = build_from_db(db_path);
        // Best-effort persist + mtime sync. None of these are
        // load-bearing for this call's return value.
        if let Ok(json) = serde_json::to_string(&built)
            && std::fs::write(&cache_path, json).is_ok()
            && let Ok(db_meta) = std::fs::metadata(db_path)
            && let Ok(modified) = db_meta.modified()
            && let Ok(f) = std::fs::File::options().write(true).open(&cache_path)
        {
            let _ = f.set_modified(modified);
        }
        built
    }
}

fn derive_cache_path(db_path: &Path) -> PathBuf {
    let stem = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "metrics.db".into());
    db_path.with_file_name(format!("{stem}.metric_labels.cache.json"))
}

fn load_if_fresh(cache_path: &Path, db_path: &Path) -> Option<MetricsCache> {
    let cm = std::fs::metadata(cache_path).ok()?;
    let dm = std::fs::metadata(db_path).ok()?;
    let cm_t = cm.modified().ok()?;
    let dm_t = dm.modified().ok()?;
    // Stale if cache is older than db. Equal mtime is fresh
    // (we set cache mtime = db mtime after each build).
    if cm_t < dm_t {
        return None;
    }
    let text = std::fs::read_to_string(cache_path).ok()?;
    serde_json::from_str(&text).ok()
}

fn build_from_db(db_path: &Path) -> MetricsCache {
    let mut out = MetricsCache::default();
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return out,
    };
    let mut stmt = match conn.prepare("SELECT spec FROM metric_instance ORDER BY spec") {
        Ok(s) => s,
        Err(_) => return out,
    };
    let mut family_set: BTreeSet<String> = BTreeSet::new();
    let mut label_set: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
        for spec in rows.flatten() {
            let (family, labels) = crate::metrics_cmd::split_spec(&spec);
            family_set.insert(family);
            for (k, v) in labels {
                label_set.entry(k).or_default().insert(v);
            }
        }
    }
    out.families = family_set.into_iter().collect();
    out.labels = label_set
        .into_iter()
        .map(|(k, vs)| (k, vs.into_iter().collect()))
        .collect();
    out
}

/// Tab-completion entry point for `nmbrs metrics match
/// <partial>`. Inspects the partial pattern to decide whether
/// the cursor is in family-name, label-key, or label-value
/// position, and returns the appropriate filtered candidates.
///
/// Position rules:
///   - No `{` in partial → family-name position; suggest
///     family names that start with the typed prefix.
///   - Inside `{...}`, current clause has no `=` → label-key
///     position; suggest label keys that start with the
///     prefix.
///   - Inside `{...}`, current clause has `=` → label-value
///     position; suggest values for that key. Quotes are
///     handled (`k="..."` form).
pub fn match_completions(partial: &str, db_path: &Path) -> Vec<String> {
    let cache = MetricsCache::load_or_build(db_path);
    match parse_position(partial) {
        Position::Family { prefix } => cache
            .families
            .into_iter()
            .filter(|f| f.starts_with(prefix))
            .collect(),
        Position::LabelKey { stable, key_prefix } => cache
            .labels
            .keys()
            .filter(|k| k.starts_with(key_prefix))
            .map(|k| format!("{stable}{k}"))
            .collect(),
        Position::LabelValue {
            stable,
            key,
            val_prefix,
            quote,
        } => {
            let Some(values) = cache.labels.get(key) else {
                return Vec::new();
            };
            values
                .iter()
                .filter(|v| v.starts_with(val_prefix))
                .map(|v| {
                    if quote {
                        format!("{stable}{v}\"")
                    } else {
                        format!("{stable}{v}")
                    }
                })
                .collect()
        }
    }
}

/// Where the cursor sits in a partial `match` pattern. The
/// `stable` prefix is the part the completion preserves
/// verbatim — bash's word-replacement model needs us to emit
/// the full token, not just the trailing piece.
#[derive(Debug)]
enum Position<'a> {
    Family {
        prefix: &'a str,
    },
    LabelKey {
        stable: String,
        key_prefix: &'a str,
    },
    LabelValue {
        stable: String,
        key: &'a str,
        val_prefix: &'a str,
        quote: bool,
    },
}

fn parse_position(partial: &str) -> Position<'_> {
    let Some(open) = partial.rfind('{') else {
        // No `{` yet — completing the family name.
        return Position::Family { prefix: partial };
    };
    let inside = &partial[open + 1..];
    // Find the start of the current clause: after the last
    // `,` inside the braces (clauses are comma-separated).
    let clause_start = inside.rfind(',').map(|p| p + 1).unwrap_or(0);
    let stable = &partial[..open + 1 + clause_start];
    let clause = &inside[clause_start..];
    // Skip leading whitespace inside the clause for matching;
    // include it in the stable echo.
    let trimmed = clause.trim_start();
    let ws_len = clause.len() - trimmed.len();
    let stable = format!("{stable}{}", &clause[..ws_len]);
    // Inside the clause, look for `=` — switches to value
    // position. `=~` is also a label op; treat the second
    // char as part of the operator and complete values
    // similarly.
    let after_ws = trimmed;
    if let Some(eq) = after_ws.find('=') {
        let key = after_ws[..eq].trim();
        // Operator may be `=` or `=~`.
        let after_eq = &after_ws[eq + 1..];
        let (op, val) = if let Some(stripped) = after_eq.strip_prefix('~') {
            ("=~", stripped)
        } else {
            ("=", after_eq)
        };
        // Detect surrounding quotes.
        let (quote, val_text) = if let Some(stripped) = val.strip_prefix('"') {
            (true, stripped)
        } else {
            (false, val)
        };
        let stable = format!("{stable}{key}{op}{}", if quote { "\"" } else { "" });
        return Position::LabelValue {
            stable,
            key,
            val_prefix: val_text,
            quote,
        };
    }
    Position::LabelKey {
        stable,
        key_prefix: after_ws,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_position() {
        match parse_position("recall") {
            Position::Family { prefix } => assert_eq!(prefix, "recall"),
            other => panic!("expected Family, got {other:?}"),
        }
    }

    #[test]
    fn label_key_position_after_open_brace() {
        match parse_position("recall@10.mean{") {
            Position::LabelKey { stable, key_prefix } => {
                assert_eq!(stable, "recall@10.mean{");
                assert_eq!(key_prefix, "");
            }
            other => panic!("expected LabelKey, got {other:?}"),
        }
    }

    #[test]
    fn label_key_position_with_partial() {
        match parse_position("recall{prof") {
            Position::LabelKey { stable, key_prefix } => {
                assert_eq!(stable, "recall{");
                assert_eq!(key_prefix, "prof");
            }
            other => panic!("expected LabelKey, got {other:?}"),
        }
    }

    #[test]
    fn label_key_after_comma() {
        match parse_position("recall{k=\"10\",lim") {
            Position::LabelKey { stable, key_prefix } => {
                assert_eq!(stable, "recall{k=\"10\",");
                assert_eq!(key_prefix, "lim");
            }
            other => panic!("expected LabelKey, got {other:?}"),
        }
    }

    #[test]
    fn label_value_position_unquoted() {
        match parse_position("recall{k=") {
            Position::LabelValue {
                stable,
                key,
                val_prefix,
                quote,
            } => {
                assert_eq!(stable, "recall{k=");
                assert_eq!(key, "k");
                assert_eq!(val_prefix, "");
                assert!(!quote);
            }
            other => panic!("expected LabelValue, got {other:?}"),
        }
    }

    #[test]
    fn label_value_position_quoted() {
        match parse_position("recall{k=\"1") {
            Position::LabelValue {
                stable,
                key,
                val_prefix,
                quote,
            } => {
                assert_eq!(stable, "recall{k=\"");
                assert_eq!(key, "k");
                assert_eq!(val_prefix, "1");
                assert!(quote);
            }
            other => panic!("expected LabelValue, got {other:?}"),
        }
    }

    #[test]
    fn label_value_position_substring_op() {
        match parse_position("recall{profile=~lab") {
            Position::LabelValue {
                stable,
                key,
                val_prefix,
                quote,
            } => {
                assert_eq!(stable, "recall{profile=~");
                assert_eq!(key, "profile");
                assert_eq!(val_prefix, "lab");
                assert!(!quote);
            }
            other => panic!("expected LabelValue, got {other:?}"),
        }
    }
}
