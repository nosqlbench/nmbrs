// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Multi-db merge for `nmbrs summary` and `nmbrs plot`.
//!
//! Given N metrics dbs from separate sessions of (typically)
//! the same workload, produce a single temp db whose rows
//! present them as one logical session: same `metric_instance`
//! per (metric, labels-without-session), all `sample_value`
//! rows accumulated under it. Summary and plot then run their
//! standard pipelines against the merged db, getting averages
//! / counts / aggregates across every input.
//!
//! ## Why not ATTACH and views?
//!
//! SQLite ATTACH + UNION ALL views works for read-only
//! cross-db reads, but `SqliteReporter`'s queries assume
//! `metric_instance.id` is a stable identity that joins to
//! `sample_value.instance_id`. Bridging IDs across attached
//! dbs would require rewriting every query. A temp merged db
//! keeps the existing reporter code unchanged.
//!
//! ## Session-label stripping
//!
//! Each `metric_instance.spec` carries a `session="…"` label
//! that's distinct per session by construction. Without
//! stripping it, two dbs from the same workload produce two
//! distinct `metric_instance` rows, and the summary would
//! show duplicate rows. Stripping `session=` before the
//! merge lets identical (metric, labels) collapse to one
//! row whose sample_values include every input's data.
//!
//! ## API
//!
//! [`merge_dbs`] takes a non-empty list of input db paths
//! and returns a temp file path holding the merged db. The
//! caller owns the temp file's lifetime — typically dropped
//! at process exit.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

/// Merge `inputs` into a single temp db. Returns the path of
/// the temp db. The temp file persists for the process lifetime
/// (the caller can `std::fs::remove_file` to clean up early).
///
/// Algorithm:
/// 1. Copy `inputs[0]` to a temp file (this preserves schema +
///    its rows verbatim so the schema is inherited).
/// 2. Strip `session="…"` from every `metric_instance.spec` in
///    the merged db so subsequent inserts with stripped specs
///    collide on UNIQUE(spec) and merge.
/// 3. For each remaining input, ATTACH and:
///    a. INSERT OR IGNORE every metric_instance with stripped
///      spec — duplicates are silently skipped (their data
///      lands in the existing row via remapped sample_values).
///    b. Build a remap map from src's metric_instance_id to
///      merged metric_instance_id (matched by stripped spec).
///    c. INSERT every sample_value row using the remap.
///    d. Same dedup-and-insert for label_key, label_value,
///      label_set, label_set_entry. Schema dedup paths use
///      INSERT OR IGNORE; the merge's queries don't depend
///      on label_set IDs being stable, only on
///      metric_instance.spec.
///    e. Carry forward session_metadata: stored summary and
///      plot specs are preserved (last-input wins on key
///      collision).
// The numbered/lettered ASCII outline in the doc above is a deliberate
// multi-level list; clippy's markdown heuristic mis-measures the nested
// continuation indent, so the lint is silenced here.
#[allow(clippy::doc_overindented_list_items)]
pub fn merge_dbs(inputs: &[PathBuf]) -> Result<PathBuf, String> {
    if inputs.is_empty() {
        return Err("merge_dbs: at least one input db is required".to_string());
    }
    // Two merges in one process can read the same clock tick; the
    // sequence keeps their temp files apart.
    static MERGE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp_path = std::env::temp_dir().join(format!(
        "nmbrs_merged_{}_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        MERGE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    // Step 1: byte-copy the first input.
    std::fs::copy(&inputs[0], &temp_path).map_err(|e| {
        format!(
            "copy '{}' → '{}': {e}",
            inputs[0].display(),
            temp_path.display()
        )
    })?;

    let conn = Connection::open(&temp_path).map_err(|e| format!("open merged db: {e}"))?;

    // The merged db is a disposable temp file — it is read once by the renderer
    // and never recovered after a crash — so durability buys nothing here while
    // costing an fsync per statement. With the row-wise loops below that was the
    // dominant cost: merging a 13 MB session db took minutes.
    let _ = conn.pragma_update(None, "synchronous", "OFF");

    // Step 2: strip session labels from the seed db's
    // metric_instance.spec. Done in-place so subsequent
    // inserts with stripped specs collide.
    strip_session_labels_in_place(&conn).map_err(|e| format!("strip session labels: {e}"))?;

    // Steps 3a–e: merge each remaining input.
    for src_path in &inputs[1..] {
        merge_one(&conn, src_path).map_err(|e| format!("merge '{}': {e}", src_path.display()))?;
    }

    Ok(temp_path)
}

/// Update `metric_instance.spec` rows in place, stripping
/// `session="…",` (or `,session="…"` at end-of-list) so that
/// specs from different sessions of the same workload become
/// equal.
fn strip_session_labels_in_place(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("SELECT id, spec FROM metric_instance")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    // One transaction for the whole rewrite. Per-row autocommit made this one
    // durable write per instance row.
    let tx = conn.unchecked_transaction()?;
    {
        let mut update = conn.prepare("UPDATE metric_instance SET spec = ?1 WHERE id = ?2")?;
        for (id, spec) in rows {
            let stripped = strip_session_label(&spec);
            if stripped != spec {
                update.execute(params![stripped, id])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Strip the `session="…"` label from a metric_instance spec.
/// Handles three positions: leading (`{session="x",rest}`),
/// middle (`a,session="x",rest`), trailing (`a,session="x"}`).
/// Quoted value boundaries are honored so a value containing
/// commas wouldn't fool the splitter — but in practice session
/// values are session-id strings without internal commas.
pub fn strip_session_label(spec: &str) -> String {
    let Some(open) = spec.find('{') else {
        return spec.to_string();
    };
    let Some(close) = spec.rfind('}') else {
        return spec.to_string();
    };
    if close <= open + 1 {
        return spec.to_string();
    }
    let body = &spec[open + 1..close];
    let parts: Vec<&str> = body.split(',').collect();
    let kept: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|p| {
            let p = p.trim_start();
            !p.starts_with("session=")
        })
        .collect();
    if kept.len() == parts.len() {
        return spec.to_string();
    }
    let new_body = kept.join(",");
    format!("{}{{{}}}", &spec[..open], new_body)
}

fn merge_one(merged: &Connection, src_path: &Path) -> rusqlite::Result<()> {
    merged.execute(
        "ATTACH DATABASE ? AS src",
        params![src_path.to_string_lossy().as_ref()],
    )?;
    // Every insert below runs in ONE transaction. The `sample_value` copy is one
    // statement per sample row, so autocommit made it one durable write per
    // sample — the reason a two-db merge read as a hang rather than a wait.
    //
    // The transaction is opened AFTER `ATTACH` and committed BEFORE `DETACH`:
    // SQLite rejects both statements inside a transaction. On an error return the
    // guard drops and rolls back, and `src` stays attached to a connection whose
    // temp file the caller abandons.
    let tx = merged.unchecked_transaction()?;

    // Insert metric_family rows that don't already exist
    // (UNIQUE(name, type) handles dedup).
    merged.execute(
        "INSERT OR IGNORE INTO main.metric_family (name, type, unit, help) \
         SELECT name, type, unit, help FROM src.metric_family",
        [],
    )?;

    // metric_instance: denormalised schema — identity is
    // `metric_instance.spec` (UNIQUE). Strip `session=…`
    // before lookup so cross-session merges collapse onto a
    // single instance per logical label set. We do this
    // row-by-row because the spec rewrite isn't expressible
    // as plain SQL on this sqlite build (no regex_replace),
    // and instance counts are small.
    let mut select = merged.prepare(
        "SELECT mi.id, mi.spec, mf.name, mf.type \
         FROM src.metric_instance mi \
         JOIN src.metric_family mf ON mi.family_id = mf.id",
    )?;
    let src_rows: Vec<(i64, String, String, String)> = select
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    drop(select);

    let mut insert = merged.prepare(
        "INSERT OR IGNORE INTO main.metric_instance (family_id, spec) \
         VALUES (\
           (SELECT id FROM main.metric_family WHERE name = ?1 AND type = ?2), \
           ?3)",
    )?;
    let mut find_merged_id =
        merged.prepare("SELECT id FROM main.metric_instance WHERE spec = ?1")?;
    let mut copy_labels = merged.prepare(
        "INSERT OR IGNORE INTO main.instance_label (instance_id, key, value) \
         SELECT ?1, key, value FROM src.instance_label WHERE instance_id = ?2",
    )?;

    let mut remap: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for (src_id, spec, fam_name, fam_type) in src_rows {
        let stripped = strip_session_label(&spec);
        insert.execute(params![fam_name, fam_type, &stripped])?;
        let merged_id: i64 = find_merged_id.query_row(params![&stripped], |r| r.get(0))?;
        copy_labels.execute(params![merged_id, src_id])?;
        remap.insert(src_id, merged_id);
    }
    drop(insert);
    drop(find_merged_id);
    drop(copy_labels);

    // sample_value: insert every row with remapped instance_id.
    let mut select_sv = merged.prepare(
        "SELECT instance_id, timestamp_ms, interval_ms, count, sum, min, max, mean, \
                stddev, p50, p75, p90, p95, p98, p99, p999 \
         FROM src.sample_value",
    )?;
    let mut insert_sv = merged.prepare(
        "INSERT INTO main.sample_value \
         (instance_id, timestamp_ms, interval_ms, count, sum, min, max, mean, \
          stddev, p50, p75, p90, p95, p98, p99, p999) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )?;
    let mut sv_iter = select_sv.query([])?;
    while let Some(r) = sv_iter.next()? {
        let src_id: i64 = r.get(0)?;
        let Some(&new_id) = remap.get(&src_id) else {
            continue;
        };
        insert_sv.execute(params![
            new_id,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<f64>>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, Option<f64>>(6)?,
            r.get::<_, Option<f64>>(7)?,
            r.get::<_, Option<f64>>(8)?,
            r.get::<_, Option<f64>>(9)?,
            r.get::<_, Option<f64>>(10)?,
            r.get::<_, Option<f64>>(11)?,
            r.get::<_, Option<f64>>(12)?,
            r.get::<_, Option<f64>>(13)?,
            r.get::<_, Option<f64>>(14)?,
            r.get::<_, Option<f64>>(15)?,
        ])?;
    }
    drop(sv_iter);
    drop(select_sv);
    drop(insert_sv);

    // session_metadata: preserve every key. Last-input wins on
    // collisions so the user can layer override specs by db
    // ordering.
    merged.execute(
        "INSERT OR REPLACE INTO main.session_metadata (key, value) \
         SELECT key, value FROM src.session_metadata",
        [],
    )?;

    tx.commit()?;
    merged.execute("DETACH DATABASE src", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two populated dbs actually merge: samples from both survive, and the
    /// same logical instance from two sessions collapses onto one row.
    ///
    /// Every other test here checks `strip_session_label` on strings — nothing
    /// ran a merge, which is how a merge slow enough to look like a hang went
    /// unnoticed. This also pins the transaction restructuring: `ATTACH` and
    /// `DETACH` now bracket a transaction, and SQLite rejects either inside one,
    /// so a mistake there fails this test rather than only slow paths in the
    /// field.
    #[test]
    fn merges_two_populated_dbs() {
        use nmbrs_metrics::labels::Labels;
        use nmbrs_metrics::reporters::sqlite::SqliteReporter;
        use nmbrs_metrics::scheduler::Reporter;
        use nmbrs_metrics::snapshot::MetricSet;
        use std::time::{Duration, Instant};

        let dir =
            std::env::temp_dir().join(format!("nmbrs-merge-test-{}", crate::scratch_suffix()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut paths = Vec::new();
        for (session, value) in [("s1", 0.80_f64), ("s2", 0.90)] {
            let db = dir.join(format!("{session}.db"));
            {
                let mut reporter = SqliteReporter::new(&db).unwrap();
                let mut snap = MetricSet::new(Duration::from_secs(1));
                // Same logical instance in both, differing only by `session`.
                snap.insert_gauge(
                    "recall_mean",
                    Labels::of("session", session).with("k", "10"),
                    value,
                    Instant::now(),
                );
                reporter.report(&snap);
                reporter.flush();
            }
            paths.push(db);
        }

        let merged = merge_dbs(&paths).expect("merge should succeed");
        let conn = Connection::open(&merged).unwrap();

        let instances: i64 = conn
            .query_row(
                "SELECT count(*) FROM metric_instance WHERE spec LIKE 'recall_mean%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            instances, 1,
            "the two sessions' identical label set must collapse to one instance"
        );

        let samples: i64 = conn
            .query_row("SELECT count(*) FROM sample_value", [], |r| r.get(0))
            .unwrap();
        assert_eq!(samples, 2, "both sessions' samples must survive the merge");

        let specs: Vec<String> = conn
            .prepare("SELECT spec FROM metric_instance")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            specs.iter().all(|s| !s.contains("session=")),
            "session labels must be stripped on both sides: {specs:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&merged);
    }

    #[test]
    fn strips_session_label_in_middle() {
        let spec = "recall@10.mean{session=\"abc\",profile=\"label_03\",k=\"10\"}";
        let stripped = strip_session_label(spec);
        assert_eq!(stripped, "recall@10.mean{profile=\"label_03\",k=\"10\"}");
    }

    #[test]
    fn strips_session_label_at_start() {
        let spec = "metric{session=\"x\",a=\"1\"}";
        assert_eq!(strip_session_label(spec), "metric{a=\"1\"}");
    }

    #[test]
    fn no_change_when_no_session_label() {
        let spec = "metric{profile=\"a\",k=\"10\"}";
        assert_eq!(strip_session_label(spec), spec);
    }

    #[test]
    fn no_change_when_no_braces() {
        assert_eq!(strip_session_label("metric"), "metric");
    }
}
