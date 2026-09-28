// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs summary` — render a summary report from any
//! `metrics.db` produced by a previous run.
//!
//! Internally calls the same
//! [`nmbrs_metrics::reporters::sqlite::SqliteReporter::format_summary`]
//! that the workload-end-of-run path uses (via
//! [`nmbrs_runtime::runner::report_config_from_summary`]). Two
//! call sites, one source of truth for what a summary looks
//! like.
//!
//! Usage:
//!
//! ```text
//!   nmbrs summary                                # list stored
//!   nmbrs summary all                            # render every stored
//!   nmbrs summary --name recall_v1               # render stored by name
//!   nmbrs summary "recall; mean(recall) over profile~label"
//!   nmbrs summary "*"                            # all-metrics ad-hoc
//!   nmbrs summary --name recall_v1 --create "recall; mean(recall)"
//!                                               # persist + render
//!   nmbrs summary --db /path/to/metrics.db ...   # override db
//!   nmbrs summary --format csv --output out.csv  # ad-hoc with format
//! ```
//!
//! Defaults:
//! - `--db`: `logs/latest/metrics.db` (the symlink the runner
//!   refreshes after each session — same path the in-run
//!   summary picks up).
//! - `--format`: `md` (Markdown table; matches what the runner
//!   produces today). Override per-call with `--format md|csv`.
//! - `--output`: `<db_dir>/<basename>_summary.<format>`. A
//!   bare basename in `--output` gets the format-derived
//!   extension appended; a path with an extension is used
//!   verbatim.
//!
//! Five resolution modes:
//!
//! 1. **Bare** (`nmbrs summary`) — list every stored named
//!    summary in the db so the user can pick (or hint at
//!    literal-spec usage when the db has none).
//! 2. **All stored** (`summary all`) — render every named
//!    summary persisted into the db's `session_metadata`
//!    table. Format and output filename derive from each
//!    stored name (e.g. `recallnmore.csv` → CSV).
//! 3. **Stored by name** (`summary --name <NAME>`) —
//!    regenerate that single named report. Errors if no
//!    stored entry matches.
//! 4. **Ad-hoc literal spec** (`summary "<spec text>"` with
//!    no `--name`) — render a one-off report. `*` is just
//!    "all metrics" in the DSL and routes here.
//! 5. **Persist + render** (`summary --name <NAME> --create
//!    "<spec>"`) — saves the spec into `session_metadata`
//!    under `summary.<NAME>` and renders it. Future
//!    `summary --name <NAME>` calls replay it.

use std::path::{Path, PathBuf};

use nmbrs_metrics::reporters::sqlite::{SqliteReporter, derive_name_and_format};
use nmbrs_runtime::runner::report_config_from_summary;
use nmbrs_workload::model::SummaryConfig;

/// Best-effort lookup of stored summary names from a metrics
/// db. Returns an empty Vec when the path doesn't exist or the
/// db can't be opened — callers (e.g. shell completion) read
/// this before any user action and shouldn't surface partial
/// failures.
pub fn list_stored_summary_names(db_path: &Path) -> Vec<String> {
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(reporter) = SqliteReporter::new(db_path) else {
        return Vec::new();
    };
    reporter
        .read_stored_summaries()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Best-effort lookup of named summaries declared in a workload
/// YAML's `summary:` block. Same shape as
/// [`list_stored_summary_names`] but sourced from the file —
/// useful for `nmbrs summary --name <TAB> workload=…` when no
/// matching session has been recorded yet.
pub fn list_workload_summary_names(workload_path: &Path) -> Vec<String> {
    load_workload_summaries(workload_path)
        .map(|specs| specs.into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default()
}

/// Read a workload YAML's `report:` block and return
/// `(name, spec_text)` pairs for every `table` item, in
/// declaration order (SRD-46).
fn load_workload_summaries(path: &Path) -> Result<Vec<(String, String)>, String> {
    let workload =
        nmbrs_workload::parse::parse_workload_from_path(path, &std::collections::HashMap::new())
            .map_err(|e| format!("parse: {e}"))?;
    let entries: Vec<(String, String)> = workload
        .report
        .items()
        .filter(|i| matches!(i.kind, nmbrs_workload::report::Kind::Table))
        .map(|i| (i.name.clone(), i.body.clone()))
        .collect();
    Ok(entries)
}

/// Render a SRD-46 v2 metricsql-driven table. Each entry in
/// `cfg.metricsql_columns` is a `(column_name, expression)`
/// pair; each is evaluated independently against the session
/// db's `SqliteDataSource`, then the results are joined on the
/// `cfg.group_by` label to produce a row per distinct group.
///
/// `format` is `md` (markdown table) or `csv`. Other formats
/// fall back to markdown — same convention as the legacy path.
/// Compare two `|`-joined composite group keys position by
/// position. Each segment uses [`natural_cmp_one`] so numeric
/// components order by magnitude (`2 < 10`) while non-numeric
/// stay lexicographic.
fn natural_cmp_pipe_tuple(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.split('|');
    let mut bi = b.split('|');
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => match natural_cmp_one(x, y) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            },
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Compare two single tokens with natural (human) ordering: digit runs compare by
/// magnitude, everything else lexicographically.
///
/// Previously this only compared numerically when the token was WHOLLY numeric
/// and fell back to plain `str::cmp` otherwise — so any label with a number
/// embedded in text sorted lexically, and rows came out
/// `0, 1, 10, 11, … 2, 20`. That is the common case, not the exception: phase
/// coordinates (`Partition(10/36 …)`), sweep labels (`k=10` vs `k=2`) and tier
/// names all embed their number. The doc claimed magnitude ordering; now the code
/// does it.
///
/// Digit runs are compared by significant length then bytes rather than by
/// parsing, so arbitrarily long numeric runs cannot overflow or lose precision,
/// and leading zeros do not change the order (`007` == `7`).
fn natural_cmp_one(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Wholly-numeric fast path first, so decimals keep float semantics
    // (`1.5` < `10`, which segment-wise comparison alone would get wrong).
    if let (Ok(x), Ok(y)) = (a.parse::<f64>(), b.parse::<f64>()) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    let (mut ar, mut br) = (a, b);
    loop {
        if ar.is_empty() || br.is_empty() {
            // The shorter prefix sorts first (`tier2` before `tier2a`).
            return ar.len().cmp(&br.len());
        }
        let a_digit = ar.starts_with(|c: char| c.is_ascii_digit());
        let b_digit = br.starts_with(|c: char| c.is_ascii_digit());
        if a_digit && b_digit {
            let an = ar.find(|c: char| !c.is_ascii_digit()).unwrap_or(ar.len());
            let bn = br.find(|c: char| !c.is_ascii_digit()).unwrap_or(br.len());
            let (ad, bd) = (&ar[..an], &br[..bn]);
            let (at, bt) = (ad.trim_start_matches('0'), bd.trim_start_matches('0'));
            match at.len().cmp(&bt.len()).then_with(|| at.cmp(bt)) {
                Ordering::Equal => {}
                non_eq => return non_eq,
            }
            ar = &ar[an..];
            br = &br[bn..];
        } else {
            let (ac, bc) = (ar.chars().next().unwrap(), br.chars().next().unwrap());
            match ac.cmp(&bc) {
                Ordering::Equal => {}
                non_eq => return non_eq,
            }
            ar = &ar[ac.len_utf8()..];
            br = &br[bc.len_utf8()..];
        }
    }
}

/// A table rendered for both destinations it has.
pub(crate) struct TableRendering {
    /// Plain-text form for the terminal: heading words stacked one per line.
    pub console: String,
    /// Valid GFM for the `.md` artifact: one header row, `<br>`-wrapped headings.
    pub markdown: String,
}

fn render_metricsql_table(
    db_path: &Path,
    cfg: &SummaryConfig,
    format: &str,
    // Table name, for diagnostics only — a `group_by` that matches nothing is
    // reported against the table the operator named.
    table_name: &str,
) -> Result<TableRendering, String> {
    use nmbrs_metrics::queryapi::sqlite::SqliteDataSource;
    use nmbrs_metricsql::eval::{EvalContext, evaluate};
    use std::collections::BTreeMap;

    // SRD-77 — execution selection from the table's `executions:`
    // directive (default per-instance-latest); the DataSource applies
    // it, so no single-`exec_id` injection.
    let selection = cfg
        .raw
        .lines()
        .find_map(|l| l.trim().strip_prefix("executions:"))
        .map(|v| crate::plot_metrics::parse_execution_selection(v.trim()))
        .transpose()?
        .unwrap_or(nmbrs_metrics::queryapi::sqlite::ExecutionSelection::LatestPerInstance);
    let ds = SqliteDataSource::open(db_path)
        .map_err(|e| format!("open metricsql sqlite adapter: {e}"))?
        .with_execution_selection(selection);
    // Anchor the instant query at the latest sample in the db
    // with a wide lookback so cadence-skewed gauge writes still
    // resolve. Same anchor logic as `plot_metrics::rows_via_metricsql`.
    let conn = rusqlite::Connection::open(db_path).map_err(|e| format!("open db: {e}"))?;
    let (min_ts, max_ts): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(MIN(timestamp_ms), 0), COALESCE(MAX(timestamp_ms), 0) \
         FROM sample_value",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("read time bounds: {e}"))?;
    if max_ts == 0 {
        return Ok(TableRendering {
            console: String::new(),
            markdown: String::new(),
        });
    }
    let ctx = EvalContext {
        data: &ds,
        start_ms: min_ts,
        end_ms: max_ts,
        step_ms: 60_000,
        lookback_ms: Some(300_000),
        query_start_ms: Some(min_ts),
        query_end_ms: Some(max_ts),
    };

    // Evaluate each column expression. For each query, build a
    // `group_value -> column_value` map. When `group_by` is
    // empty, every row collapses into a single un-named row;
    // we put it under the empty string for stable iteration.
    // Multi-key `group_by` produces a tuple key (the row's
    // label values joined by `|`) so the table breaks down
    // along the same dimensions the plot's series do.
    let group_keys: &[String] = cfg.group_by.as_slice();
    let mut by_group: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    // Label keys the results actually carry, and how many series carried each
    // `group_by` key. Collected during the fold that already walks every
    // series, so the check below costs a set insert per label.
    let mut labels_present: std::collections::BTreeSet<String> = Default::default();
    let mut group_key_hits: BTreeMap<String, usize> = BTreeMap::new();
    let mut series_seen: usize = 0;
    let n_cols = cfg.metricsql_columns.len();
    for (col_idx, (_col_name, expr)) in cfg.metricsql_columns.iter().enumerate() {
        let parsed = nmbrs_metricsql::parse(expr).map_err(|e| format!("parse '{expr}': {e}"))?;
        let series = evaluate(&ctx, &parsed).map_err(|e| format!("evaluate '{expr}': {e}"))?;
        for s in series {
            series_seen += 1;
            for (k, _) in &s.labels {
                labels_present.insert(k.clone());
            }
            for key in group_keys {
                if s.labels.iter().any(|(k, _)| k == key) {
                    *group_key_hits.entry(key.clone()).or_insert(0) += 1;
                }
            }
            let group_val: String = if group_keys.is_empty() {
                String::new()
            } else {
                group_keys
                    .iter()
                    .map(|polydat| {
                        s.labels
                            .iter()
                            .find(|(k, _)| k == polydat)
                            .map(|(_, v)| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            };
            // The cell is the value the COLUMN'S EXPRESSION computed — the
            // last sample it produced. The renderer does not reduce.
            //
            // It used to take the mean of every sample in the window, to make
            // a table cell match the curve of a companion plot. That silently
            // overrode what each column asked for: `max(compaction_completion_ratio)
            // by (p)` evaluates to 1.0 for a finished tier and the table
            // rendered 99.96, because the mean folded in the samples from
            // while it was still running. No expression could win, since the
            // averaging happened after evaluation.
            //
            // There is no need for a per-column aggregate setting either: the
            // MetricsQL engine already spells all of them —
            // `avg_over_time(x[5m])`, `min_over_time`, `max_over_time`,
            // `last_over_time`. A column that wants the plot's mean asks for
            // it; a column that wants the current value says nothing and gets
            // the last sample, which is what an instant query returns anyway.
            let value = s
                .samples
                .iter()
                .rev()
                .map(|s| s.value)
                .find(|v| v.is_finite());
            let row = by_group
                .entry(group_val)
                .or_insert_with(|| vec![None; n_cols]);
            row[col_idx] = value;
        }
    }

    // Completion state per row (`state: <expr>`). Evaluated like a value column
    // but never displayed as one: presence of a value means the row finished,
    // absence means it is still going. Presence is the test rather than a
    // threshold on progress, because a progress gauge keeps its last polled value
    // after the work ends and can sit below 100 on a row that completed.
    // A `group_by` key that no result carries is almost always a rename or a
    // typo, and it fails SILENTLY: the fold above resolves a missing label to
    // "" (`unwrap_or("")`), so every series lands under one key and the table
    // renders a single row that LOOKS like data. That is how a tier label
    // renamed `p` -> `part` turned 20 tiers into one blank-keyed row with no
    // indication anything was wrong.
    //
    // Listing the labels that ARE present is the load-bearing half — seeing
    // `p` sitting next to a `group_by: part` is the whole diagnosis.
    //
    // A warning, not an error: mid-run a metric may not have been emitted
    // yet, and a table that renders with a caveat beats one that refuses.
    for key in group_keys {
        match group_key_hits.get(key).copied().unwrap_or(0) {
            0 => {
                // The evaluated series are no help here: `… by (part)` strips
                // every label the aggregation did not group on, so a result
                // that failed to group carries no labels at all. Ask the
                // session what labels its instances actually have — that is
                // the list with `p` in it, which is the answer.
                let available: Vec<String> = conn
                    .prepare("SELECT DISTINCT key FROM instance_label ORDER BY key")
                    .and_then(|mut st| {
                        st.query_map([], |r| r.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap_or_default();
                let present = if available.is_empty() {
                    labels_present.iter().cloned().collect::<Vec<_>>()
                } else {
                    available
                };
                eprintln!(
                    "nmbrs summary: table '{table_name}' groups by `{key}`, but no \
                     result series carries that label — {series} series collapsed \
                     into {rows} row(s). Labels in this session: {present}",
                    series = series_seen,
                    rows = by_group.len(),
                    present = present.join(", "),
                );
            }
            hits if hits < series_seen => eprintln!(
                "nmbrs summary: table '{table_name}' groups by `{key}`, but only \
                 {hits} of {series_seen} result series carry it; the rest are \
                 grouped under an empty key."
            ),
            _ => {}
        }
    }

    let state_by_group: std::collections::BTreeMap<String, bool> = match &cfg.state_query {
        None => Default::default(),
        Some(expr) => {
            let parsed =
                nmbrs_metricsql::parse(expr).map_err(|e| format!("parse state '{expr}': {e}"))?;
            let series =
                evaluate(&ctx, &parsed).map_err(|e| format!("evaluate state '{expr}': {e}"))?;
            let mut m: std::collections::BTreeMap<String, bool> = Default::default();
            for s in series {
                let group_val: String = if group_keys.is_empty() {
                    String::new()
                } else {
                    group_keys
                        .iter()
                        .map(|k| {
                            s.labels
                                .iter()
                                .find(|(lk, _)| lk == k)
                                .map(|(_, v)| v.clone())
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                };
                let done = s.samples.iter().any(|sm| sm.value.is_finite());
                let e = m.entry(group_val).or_insert(false);
                *e = *e || done;
            }
            m
        }
    };

    // Per-column unit / scaling decision. For columns whose
    // query targets a time-domain metric (latency,
    // servicetime, anything ending in `_ns`/`_seconds`/`_us`/
    // `_ms`), pick a uniform display unit from the column's
    // own value range so cells are read-able and the unit
    // is shown in the heading. Without this, the operator
    // sees a column of bare nanosecond integers labelled
    // `latency` and has no way to tell whether
    // `338422551` is microseconds, nanoseconds, or seconds.
    let timestamp_columns: Vec<bool> = cfg
        .metricsql_columns
        .iter()
        .map(|(_name, expr)| is_timestamp_query(expr))
        .collect();
    let column_si: Vec<Option<SiScale>> = cfg
        .metricsql_columns
        .iter()
        .enumerate()
        .map(|(idx, (_name, expr))| {
            // Durations and moments have their own scales; percentages are
            // already human-sized and a `K` on a percent column reads as an error.
            if timestamp_columns[idx] || is_time_domain_query(expr) {
                return None;
            }
            // Seconds-domain columns get their own scale below.
            if is_seconds_domain_query(expr) {
                return None;
            }
            let name_l = cfg.metricsql_columns[idx].0.to_ascii_lowercase();
            if name_l.contains("pct") || name_l.contains("percent") || name_l.contains("ratio") {
                return None;
            }
            let max_abs = by_group
                .values()
                .filter_map(|row| row[idx])
                .filter(|v| v.is_finite())
                .fold(0.0_f64, |m, v| m.max(v.abs()));
            SiScale::for_max(max_abs)
        })
        .collect();
    let column_secs: Vec<bool> = cfg
        .metricsql_columns
        .iter()
        .map(|(_name, expr)| is_seconds_domain_query(expr))
        .collect();
    let column_units: Vec<Option<TimeUnit>> = cfg
        .metricsql_columns
        .iter()
        .enumerate()
        .map(|(idx, (_name, expr))| {
            // A moment is formatted as a clock time, not scaled as a duration.
            if timestamp_columns[idx] {
                return None;
            }
            if !is_time_domain_query(expr) {
                return None;
            }
            // Gather the max-abs cell value across rows.
            let max_abs = by_group
                .values()
                .filter_map(|row| row[idx])
                .fold(0.0_f64, |m, v| m.max(v.abs()));
            Some(TimeUnit::for_max_nanos(max_abs))
        })
        .collect();

    // Natural-order the rows so the table reads in the same
    // sequence as the plot (numeric x values ascend by
    // magnitude, not by string order — `1, 2, 4, 8, 16`
    // instead of `1, 16, 2, 32, 4`). Each composite group_val
    // is split on `|` and the components are compared
    // position-by-position with `natural_cmp`.
    let mut by_group: Vec<(String, Vec<Option<f64>>)> = by_group.into_iter().collect();
    by_group.sort_by(|a, b| natural_cmp_pipe_tuple(&a.0, &b.0));

    // Render headers including unit annotations.
    let column_headers: Vec<String> = cfg
        .metricsql_columns
        .iter()
        .zip(column_units.iter())
        .enumerate()
        .map(|(idx, ((name, _expr), unit))| {
            // `header <col>: <note>` — the column's definition joins
            // the header stack under its name (word-wrap turns each
            // space into a header line), so the table explains its
            // own values where the reader is looking.
            let name: String = match cfg.header_notes.iter().find(|(c, _)| c == name) {
                Some((_, note)) => format!("{name} {note}"),
                None => name.clone(),
            };
            if timestamp_columns[idx] {
                return format!("{name} (UTC)");
            }
            match unit {
                Some(u) => format!("{name} ({})", u.symbol),
                None => match (column_secs[idx], column_si[idx]) {
                    (true, _) => format!("{name} (h:m:s)"),
                    (false, Some(si)) => format!("{name} ({})", si.symbol),
                    (false, None) => name,
                },
            }
        })
        .collect();

    // Render a single cell against the chosen column unit.
    fn render_cell(
        value: Option<f64>,
        unit: Option<&TimeUnit>,
        si: Option<SiScale>,
        secs: bool,
        is_timestamp: bool,
        sep: &str,
    ) -> String {
        let _ = sep;
        if is_timestamp {
            return match value {
                None => "-".to_string(),
                Some(v) => nmbrs_runtime::session::format_utc_short(v),
            };
        }
        match (value, unit) {
            (None, _) => "-".to_string(),
            (Some(v), Some(u)) => format_sig(v / u.divisor),
            (Some(v), None) => match (secs, si) {
                (true, _) => format_hms(v),
                (false, Some(si)) => format_sig(v / si.divisor),
                (false, None) => format_sig(v),
            },
        }
    }

    // Emit. Markdown table by default; CSV with `--format=csv`.
    // Group columns: one header column per key in
    // `group_keys`; cell values come from splitting the
    // composite `group_val` on `|`.
    let split_group = |g: &str| -> Vec<String> {
        if group_keys.is_empty() {
            Vec::new()
        } else {
            g.split('|').map(str::to_string).collect()
        }
    };
    if format.eq_ignore_ascii_case("csv") {
        let mut out = String::new();
        let mut header: Vec<&str> = group_keys.iter().map(String::as_str).collect();
        let header_strs: Vec<&str> = column_headers.iter().map(String::as_str).collect();
        for h in &header_strs {
            header.push(*h);
        }
        out.push_str(&header.join(","));
        out.push('\n');
        for (group_val, cells) in &by_group {
            let mut row: Vec<String> = split_group(group_val);
            for (idx, (cell, unit)) in cells.iter().zip(column_units.iter()).enumerate() {
                row.push(render_cell(
                    *cell,
                    unit.as_ref(),
                    column_si[idx],
                    column_secs[idx],
                    timestamp_columns[idx],
                    ",",
                ));
            }
            out.push_str(&row.join(","));
            out.push('\n');
        }
        // CSV is not markdown — the same text serves both destinations.
        return Ok(TableRendering {
            console: out.clone(),
            markdown: out,
        });
    }

    // Markdown. Label columns left-aligned, value columns right-aligned; the
    // shared renderer pads every cell so the `|` grid lines up in the raw file.
    let has_state = cfg.state_query.is_some();
    let header: Vec<String> = group_keys
        .iter()
        .cloned()
        .chain(has_state.then(|| "state".to_string()))
        .chain(column_headers.iter().cloned())
        .collect();
    // The state word is a label, so it left-aligns with the group keys rather
    // than right-aligning with the numbers.
    let label_cols = group_keys.len() + usize::from(has_state);
    let rows: Vec<Vec<String>> =
        by_group
            .iter()
            .map(|(group_val, cells)| {
                let mut row = split_group(group_val);
                if has_state {
                    row.push(match state_by_group.get(group_val) {
                        Some(true) => "complete".to_string(),
                        _ => "active".to_string(),
                    });
                }
                row.extend(cells.iter().zip(column_units.iter()).enumerate().map(
                    |(idx, (c, u))| {
                        render_cell(
                            *c,
                            u.as_ref(),
                            column_si[idx],
                            column_secs[idx],
                            timestamp_columns[idx],
                            " | ",
                        )
                    },
                ));
                row
            })
            .collect();
    // Two renderings of the same table from one query pass: the console form
    // stacks heading words on their own lines (narrow, aligned as plain text),
    // and the markdown form is a valid GFM table whose headings wrap via `<br>`.
    // Neither is derivable from the other by string surgery, and re-querying to
    // produce the second would double the work for a formatting difference.
    Ok(TableRendering {
        console: crate::report::markdown_table(&header, &rows, label_cols),
        markdown: crate::report::markdown_table_gfm(&header, &rows, label_cols),
    })
}

/// Display unit for a time-domain column. nmbrs internal
/// metrics are nanoseconds (per project memory
/// "Nanos Standard"); the formatter picks a single
/// human-readable unit per *column* so cells stay aligned
/// and the unit shows up in the heading once.
#[derive(Copy, Clone, Debug)]
struct TimeUnit {
    /// `s` / `ms` / `µs` / `ns`.
    symbol: &'static str,
    /// What to divide nanoseconds by to produce the
    /// displayed value.
    divisor: f64,
}

/// Column scale for a value already expressed in SECONDS.
///
/// The duration machinery below assumes nanoseconds (nmbrs metrics are nanos by
/// convention), but a difference of two `t*_over_time` moments is seconds. Left
/// to the SI scale such a column reads "2.00 (K)" — kiloseconds, which is
/// arithmetically right and useless to a human watching a compaction.
/// Elapsed time is written as a clock, not as a scaled number.
///
/// "4.50 (min)" asks the reader to convert; a scale that changes with the
/// column's magnitude asks them to check the heading first, and one tier at
/// "0.50 (min)" beside another at "4.50 (min)" hides that these are 30 seconds
/// and 4½ minutes. `HH:MM:SS` is unambiguous, sorts correctly, and every row
/// carries the same shape.
/// Render a measurement at four significant digits, without trailing noise.
///
/// Fixed decimals were the wrong tool: `{:.4}` renders a count of two segments
/// as `2.0000` and a completion of 99.88% as `99.8788`, spending four digits on
/// precision the measurement does not have and burying the magnitude that does
/// matter. Significant digits track the value instead of the format string —
/// `2`, `99.88`, `0.3410`, `124.5`.
///
/// Whole numbers render without a decimal point at all, so a count reads as a
/// count.
fn format_sig(v: f64) -> String {
    if !v.is_finite() {
        return "-".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    // An exact integer is a count (or has been rounded to one) — never dress it
    // up with a fractional part it does not have.
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    const SIG: i32 = 4;
    let magnitude = v.abs().log10().floor() as i32;
    // Digits after the point that leave SIG significant ones. Clamped at 0 so a
    // large value never grows a fractional tail, and at 6 so a very small one
    // does not run away.
    let decimals = (SIG - 1 - magnitude).clamp(0, 6) as usize;
    let rendered = format!("{v:.decimals$}");
    // Trim the zeros that rounding leaves behind (`0.3410` stays, `2.5000`
    // becomes `2.5`), but never leave a bare trailing point.
    if rendered.contains('.') {
        let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');
        return trimmed.to_string();
    }
    rendered
}

fn format_hms(total_seconds: f64) -> String {
    if !total_seconds.is_finite() || total_seconds < 0.0 {
        return "-".to_string();
    }
    let secs = total_seconds.round() as u64;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Whether a column's value is a DURATION in seconds: a moment arithmetic'd
/// against another moment. `is_timestamp_query` rejects these as not-a-moment;
/// this catches what that rejection leaves behind.
fn is_seconds_domain_query(expr: &str) -> bool {
    let lower = expr.to_ascii_lowercase();

    // Rule 2 — an explicit millisecond→second conversion: the whole expression
    // divided by a bare literal 1000. Narrow on purpose. `live_bytes_per_ms`
    // also contains "1000", but as `/ (1000 * (…))` — a parenthesised divisor,
    // not a trailing literal — so it stays a rate, which is the distinction the
    // rest of this function exists to protect.
    if let Some(head) = lower.trim_end().strip_suffix("1000") {
        let head = head.trim_end();
        if let Some(before) = head.strip_suffix('/') {
            let mut depth = 0i32;
            for c in before.chars() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
            }
            if depth == 0 {
                return true;
            }
        }
    }

    const MOMENT_FNS: &[&str] = &[
        "tfirst_over_time",
        "tlast_over_time",
        "tlast_change_over_time",
    ];
    if !MOMENT_FNS.iter().any(|f| lower.contains(f)) {
        return false;
    }
    // Seconds come from SUBTRACTING one moment from another. A rate that merely
    // DIVIDES by such a difference is not itself a duration — it carries the
    // units of its numerator, and labelling one "(h)" because the elapsed time
    // appears in its denominator turns bytes-per-millisecond into hours.
    // Unwrap whole-expression parens first so `(tlast(...) - tfirst(...))`
    // classifies the same as its bare form.
    let mut depth = 0i32;
    for c in strip_outer_parens(lower.trim()).chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '-' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// A magnitude shared by every cell in one column.
///
/// Chosen from the column's largest value and then applied to ALL of its rows,
/// so the rows stay comparable at a glance: a column of byte counts reads
/// 0.49 / 2.45 / 19.59 under a `G` heading, not 489170772 beside 19594373133
/// where the eye has to count digits to see which is bigger. Mixing magnitudes
/// within a column would defeat that, which is why the scale is a property of
/// the column rather than of each value.
#[derive(Copy, Clone, Debug, PartialEq)]
struct SiScale {
    symbol: &'static str,
    divisor: f64,
}

impl SiScale {
    fn for_max(max_abs: f64) -> Option<Self> {
        // Below a thousand there is nothing to gain: the number is already
        // readable and a suffix would only add a decimal point.
        if !max_abs.is_finite() {
            None
        } else if max_abs >= 1e12 {
            Some(Self {
                symbol: "T",
                divisor: 1e12,
            })
        } else if max_abs >= 1e9 {
            Some(Self {
                symbol: "G",
                divisor: 1e9,
            })
        } else if max_abs >= 1e6 {
            Some(Self {
                symbol: "M",
                divisor: 1e6,
            })
        } else if max_abs >= 1e3 {
            Some(Self {
                symbol: "K",
                divisor: 1e3,
            })
        } else {
            None
        }
    }
}

impl TimeUnit {
    fn for_max_nanos(max_abs: f64) -> Self {
        if max_abs >= 1e9 {
            Self {
                symbol: "s",
                divisor: 1e9,
            }
        } else if max_abs >= 1e6 {
            Self {
                symbol: "ms",
                divisor: 1e6,
            }
        } else if max_abs >= 1e3 {
            Self {
                symbol: "µs",
                divisor: 1e3,
            }
        } else {
            Self {
                symbol: "ns",
                divisor: 1.0,
            }
        }
    }
}

/// Heuristic: does this metricsql query target a time-domain
/// metric whose values are stored as nanoseconds?
///
/// We can't reach into the metric registry from the table
/// renderer, but the OpenMetrics naming convention and
/// nmbrs's own metric-name vocabulary make this an easy
/// substring match. Both internal-time names (containing
/// `latency`, `servicetime`, `duration`, `elapsed`) and the
/// suffix conventions (`_ns`, `_seconds`, `_ms`, `_us`,
/// `_µs`) are recognised. False positives (a hypothetical
/// `latency_count` of dimensionless integers) would just
/// re-scale a small column harmlessly.
/// Whether a column's value is a MOMENT rather than a quantity.
///
/// `tfirst_over_time` / `tlast_over_time` return Unix seconds, so the duration
/// heuristics below would happily label them "s" and print 1785…  as a magnitude.
/// A moment needs formatting as a clock time instead, and this test has to run
/// first because such a query also matches the `_seconds` suffix rule.
fn is_timestamp_query(expr: &str) -> bool {
    let lower = expr.to_ascii_lowercase();
    // Every MetricsQL `t*_over_time` returns a moment, so match the family rather
    // than listing members: `tlast_change_over_time` is not a substring of
    // `tlast_over_time` and was silently rendering as a raw epoch float.
    const MOMENT_FNS: &[&str] = &[
        "tfirst_over_time",
        "tlast_over_time",
        "tlast_change_over_time",
    ];
    if !MOMENT_FNS.iter().any(|f| lower.contains(f)) {
        return false;
    }
    // Only a BARE timestamp rollup is a moment. `tlast - tfirst` is an elapsed
    // duration and must scale like one; formatting it as a clock time turns
    // "5h25m of compaction" into a date in 1970. Any top-level arithmetic means
    // the value is derived, so look for an operator outside parentheses —
    // after unwrapping redundant whole-expression parens, which otherwise
    // hide the arithmetic at depth 1 (`(tlast(...) - tfirst(...))` is still
    // a duration).
    let mut depth = 0i32;
    for c in strip_outer_parens(lower.trim()).chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '+' | '-' | '*' | '/' if depth == 0 => return false,
            _ => {}
        }
    }
    true
}

/// Strip paren pairs that wrap the ENTIRE expression, so depth-0
/// operator scans see through `(a - b)`. Only a pair whose opener
/// matches the final char is removed — `(a) - (b)` is untouched.
fn strip_outer_parens(mut s: &str) -> &str {
    while s.starts_with('(') && s.ends_with(')') {
        let mut depth = 0i32;
        let mut wraps_whole = true;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && i != s.len() - 1 {
                        wraps_whole = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !wraps_whole {
            break;
        }
        s = s[1..s.len() - 1].trim();
    }
    s
}

fn is_time_domain_query(expr: &str) -> bool {
    let lower = expr.to_ascii_lowercase();
    const NAME_HINTS: &[&str] = &[
        "latency",
        "servicetime",
        "service_time",
        "duration",
        "elapsed",
        "responsetime",
        "response_time",
    ];
    if NAME_HINTS.iter().any(|h| lower.contains(h)) {
        return true;
    }
    // Suffix conventions. Look at every metric-shaped
    // token (alphanumeric + underscores) for the suffix.
    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        for suffix in ["_ns", "_seconds", "_ms", "_us"] {
            if tok.ends_with(suffix) {
                return true;
            }
        }
        // SRD-91 outcome instruments: the `result_*` summaries record
        // op service time in NANOSECONDS, so their stat columns
        // (`result_success_p99`, `_mean`, …) are durations by
        // contract. Without this, those columns fell through to the
        // generic SI scaler and rendered as tera-nanosecond
        // absurdities ("3.026 (T)" for a 50-minute p99). Bare
        // `result_success` (a count) and `_rate` (per-second) don't
        // match — only the stat suffixes.
        if let Some(stat) = tok
            .strip_prefix("result_")
            .and_then(|t| t.rsplit('_').next())
        {
            let is_pct = stat.len() >= 2
                && stat.starts_with('p')
                && stat[1..].chars().all(|c| c.is_ascii_digit());
            if is_pct || matches!(stat, "mean" | "min" | "max" | "stddev" | "sum") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod time_unit_tests {
    use super::*;

    #[test]
    fn time_unit_for_max_nanos_picks_natural_scale() {
        assert_eq!(TimeUnit::for_max_nanos(2_500_000_000.0).symbol, "s");
        assert_eq!(TimeUnit::for_max_nanos(338_422_551.0).symbol, "ms");
        assert_eq!(TimeUnit::for_max_nanos(951_290.0).symbol, "µs");
        assert_eq!(TimeUnit::for_max_nanos(750.0).symbol, "ns");
        assert_eq!(TimeUnit::for_max_nanos(0.0).symbol, "ns");
    }

    #[test]
    fn is_time_domain_query_recognises_the_canonical_names() {
        assert!(is_time_domain_query(
            "avg(cycles_servicetime_mean) by (profile)"
        ));
        assert!(is_time_domain_query("avg(latency_p99) by (profile)"));
        assert!(is_time_domain_query("avg(some_metric_ns)"));
        assert!(is_time_domain_query(
            "rate(http_request_duration_seconds[5m])"
        ));
        assert!(
            is_time_domain_query("AVG(LATENCY_MEAN)"),
            "case-insensitive"
        );
    }

    #[test]
    fn outcome_instrument_stats_are_durations() {
        // SRD-91: result_* summary stats are nanoseconds by contract.
        assert!(is_time_domain_query(
            "max(max_over_time(result_success_p99{phase=\"x\"}[30d])) by (phase)"
        ));
        assert!(is_time_domain_query("avg(result_failure_mean)"));
        // The bare counter and the per-second rate are NOT durations.
        assert!(!is_time_domain_query(
            "max(last_over_time(result_success[30d]))"
        ));
        assert!(!is_time_domain_query(
            "avg(avg_over_time(result_success_rate[30d]))"
        ));
        // recall_* stats are ratios, not times.
        assert!(!is_time_domain_query(
            "avg(avg_over_time(recall_p50[30d])) by (r)"
        ));
    }

    #[test]
    fn is_time_domain_query_rejects_dimensionless() {
        assert!(!is_time_domain_query("avg(recall_mean) by (profile)"));
        assert!(!is_time_domain_query("count(rows_total)"));
        assert!(!is_time_domain_query("max(connection_errors)"));
    }

    #[test]
    fn moment_classifiers_see_through_whole_expression_parens() {
        // A wrapped moment difference is still a DURATION, not a moment —
        // without the unwrap it rendered as a UTC date.
        let wrapped = "(max(tlast_over_time(x{p=\"a\"}[1d])) by (p) - \
                       min(tfirst_over_time(x{p=\"a\"}[1d])) by (p))";
        assert!(
            !is_timestamp_query(wrapped),
            "wrapped difference is not a moment"
        );
        assert!(
            is_seconds_domain_query(wrapped),
            "wrapped difference is seconds"
        );
        // A wrapped BARE rollup stays a moment.
        assert!(is_timestamp_query("(tlast_over_time(x[1d]))"));
        // Adjacent groups are not whole-expression wrapping.
        assert_eq!(strip_outer_parens("(a) - (b)"), "(a) - (b)");
        assert_eq!(strip_outer_parens("((a - b))"), "a - b");
    }
}

pub fn summary_command(args: &[String]) {
    let opts = parse_args(args);

    // Resolve the effective db path. With one db (or none) the
    // path is used as-is. With multiple dbs the merge step runs
    // first, producing a temp file whose merged rows feed
    // SqliteReporter as if from one logical session.
    let primary_db = opts
        .db
        .clone()
        .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db);
    let effective_dbs: Vec<PathBuf> = if opts.dbs.is_empty() {
        vec![primary_db.clone()]
    } else {
        opts.dbs.clone()
    };
    for db in &effective_dbs {
        if !db.exists() {
            eprintln!("nmbrs summary: metrics db not found at '{}'.", db.display());
            eprintln!();
            eprintln!("Did a previous run finish? `logs/latest/` should be");
            eprintln!("a symlink to the most recent session directory.");
            eprintln!("Override with --db <path>.");
            std::process::exit(1);
        }
    }
    let db_path: PathBuf = if effective_dbs.len() > 1 {
        match crate::db_merge::merge_dbs(&effective_dbs) {
            Ok(path) => {
                eprintln!("merge: {} dbs → {}", effective_dbs.len(), path.display());
                path
            }
            Err(e) => {
                eprintln!("nmbrs summary: failed to merge dbs: {e}");
                std::process::exit(1);
            }
        }
    } else {
        effective_dbs[0].clone()
    };
    // Output paths anchor on the primary (first) db, not the
    // merged temp — keeps artifacts alongside real session data.
    let output_anchor: PathBuf = effective_dbs[0].clone();

    let mut reporter = match SqliteReporter::new(&db_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nmbrs summary: failed to open '{}': {e}", db_path.display());
            std::process::exit(1);
        }
    };

    // Six cases, decided by combinations of `--name`,
    // `--create`, and the bare positional spec:
    //
    //   1. Bare `summary` (no flags, no positional)
    //                                          → error, list stored.
    //   2. `summary all` (positional only)     → every stored.
    //   3. `summary --name N` (no `--create`, no positional)
    //                                          → render stored N.
    //   4. `summary <spec>` (positional only)  → ad-hoc literal.
    //   5. `summary --name N --create <spec>`  → persist <spec>
    //      under N, then render the new entry.
    //   6. Combinations the contract forbids   → hard error.
    // Source named summaries: `workload=<path>` overrides the
    // metrics db's `session_metadata` table. Useful before any
    // run has happened, or when the workload's `summary:` block
    // has been edited and the user wants the new spec applied
    // to existing data.
    let stored: Vec<(String, String)> = match opts.workload.as_deref() {
        Some(path) => match load_workload_summaries(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("nmbrs summary: workload '{}': {e}", path.display());
                std::process::exit(1);
            }
        },
        None => reporter.read_stored_summaries(),
    };
    let to_render: Vec<(String, SummaryConfig)> =
        match (opts.name.as_deref(), opts.create, opts.spec.as_deref()) {
            // Case 6a: --create without --name has no place to
            // store the spec. Reject early with a clear message.
            (None, true, _) => {
                eprintln!("nmbrs summary: --create requires --name <NAME>");
                std::process::exit(1);
            }
            // Case 4b: --name + positional spec without --create
            // = render the ad-hoc spec under that name. The name
            // drives the standalone output filename
            // (`<name>_summary.md`) and the markdown report's
            // section identifier so concurrent ad-hoc renders
            // (e.g. SRD-46 companion tables, one per plot) get
            // distinct sections instead of stomping on a single
            // shared `default` slot. Add `--create` only when
            // you want the spec persisted to the db for replay.
            (Some(name), false, Some(spec_text)) => literal_spec(spec_text, Some(name)),
            // Case 5: persist + render.
            (Some(name), true, Some(spec_text)) => {
                let cfg = SummaryConfig::parse(spec_text);
                reporter.set_metadata(&format!("summary.{name}"), &cfg.raw);
                eprintln!(
                    "created: summary.{name} → {} (in {})",
                    cfg.raw.lines().next().unwrap_or("").trim(),
                    db_path.display()
                );
                vec![(name.to_string(), cfg)]
            }
            // Case 6c: --create --name N but no spec — nothing to
            // persist.
            (Some(_), true, None) => {
                eprintln!("nmbrs summary: --create --name <NAME> needs a positional spec");
                std::process::exit(1);
            }
            // Case 3: render stored by name.
            (Some(name), false, None) => {
                let Some((found, raw)) = stored.iter().find(|(n, _)| n == name) else {
                    eprintln!(
                        "nmbrs summary: no stored summary named '{name}' in '{}'",
                        db_path.display()
                    );
                    if !stored.is_empty() {
                        eprintln!();
                        eprintln!("Available:");
                        for (n, _) in &stored {
                            eprintln!("  {n}");
                        }
                    }
                    std::process::exit(1);
                };
                return_stored_or_literal(found, raw)
            }
            // Case 2: render every stored.
            (None, false, Some("all")) => {
                if stored.is_empty() {
                    eprintln!(
                        "nmbrs summary: '{}' has no stored named \
                           summaries to render. Use `nmbrs summary '*'` \
                           for an ad-hoc all-metrics report, or \
                           `--name <N> --create <spec>` to persist \
                           one first.",
                        db_path.display()
                    );
                    std::process::exit(1);
                }
                stored
                    .into_iter()
                    .map(|(name, raw)| (name, SummaryConfig::parse(&raw)))
                    .collect()
            }
            // Case 4: ad-hoc literal spec (no `--name`, no
            // `--create`). Includes the `*` wildcard, which is just
            // a literal spec the DSL knows how to parse.
            (None, false, Some(spec_text)) => literal_spec(spec_text, None),
            // Case 1: bare. List stored, or hint at literal-spec
            // mode if nothing is persisted yet.
            (None, false, None) => {
                if stored.is_empty() {
                    eprintln!(
                        "nmbrs summary: '{}' has no stored named \
                           summaries.",
                        db_path.display()
                    );
                    eprintln!();
                    eprintln!("Pass a literal spec to render an ad-hoc report:");
                    eprintln!("  nmbrs summary '*'                  # all metrics");
                    eprintln!("  nmbrs summary 'recall; mean(...)'  # custom DSL");
                    eprintln!();
                    eprintln!("Use `--name <N> --create <spec>` to persist a");
                    eprintln!("spec into the db so future runs can replay it.");
                } else {
                    eprintln!(
                        "nmbrs summary: '{}' has stored named summaries —",
                        db_path.display()
                    );
                    eprintln!("pick one with --name, or use `summary all` for every.");
                    eprintln!();
                    eprintln!("Available:");
                    for (name, raw) in &stored {
                        let preview = raw.lines().next().unwrap_or("").trim();
                        let preview = if preview.len() > 60 {
                            format!("{}…", &preview[..60])
                        } else {
                            preview.to_string()
                        };
                        eprintln!("  {name:<24}  {preview}");
                    }
                    eprintln!();
                    eprintln!("Examples:");
                    eprintln!("  nmbrs summary all                       # render every stored");
                    eprintln!("  nmbrs summary --name {}", stored[0].0);
                }
                std::process::exit(1);
            }
        };

    // When a single ad-hoc report is requested AND the user
    // gave `--output`, that path applies to the one report
    // (whether it has an extension or not). When multiple
    // reports are produced, `--output` would be ambiguous —
    // ignored with a warning.
    let multiple = to_render.len() > 1;
    if multiple && opts.output.is_some() {
        eprintln!(
            "warning: --output is ignored when multiple summaries \
                   are rendered; falling back to per-name filenames in \
                   the db's session directory."
        );
    }

    let cli_format = opts.format.clone();
    // SRD-46 routing, resolved once for this invocation. `none`
    // suppresses every sink — the item still resolves and still
    // reports "no rows" diagnostics, it just isn't delivered.
    let dests = opts.destinations();
    let to_session = dests.contains(&nmbrs_workload::report::Destination::SessionDir);
    let to_stdout = dests.contains(&nmbrs_workload::report::Destination::Stdout);
    let to_stderr = dests.contains(&nmbrs_workload::report::Destination::Stderr);
    let mut any_nonempty = false;
    for (name, cfg) in &to_render {
        // Format precedence: CLI `--format` wins; otherwise
        // derive from the stored name's suffix; default to md.
        let (basename, derived_format) = derive_name_and_format(name);
        let format = cli_format.clone().unwrap_or(derived_format);
        // SRD-46 v2: native metricsql tables route through a
        // dedicated renderer; legacy DSL tables stay on the
        // SqliteReporter path.
        let rendered = if !cfg.metricsql_columns.is_empty() {
            match render_metricsql_table(&db_path, cfg, &format, name) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("nmbrs summary: metricsql table '{name}' failed: {e}");
                    continue;
                }
            }
        } else {
            // SRD-77 — default to the latest execution's data
            // when the operator hasn't explicitly opted into an
            // aggregate read. The summary command operates on
            // one session's metrics.db, so "latest" means the
            // max(exec_id) recorded in that db's executions
            // table. Emit the multi-exec banner so the
            // operator sees which execution they're seeing.
            let session_dir = db_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            nmbrs_runtime::refine_plan::warn_multi_execution_default(&session_dir);
            let exec_id_filter =
                nmbrs_runtime::refine_plan::ExecutionQualifier::latest(&session_dir).specific_id();
            let report_cfg = report_config_from_summary(cfg, exec_id_filter);
            // The legacy SQL renderer has one form, used for both destinations.
            let legacy = reporter.format_summary_with_format(&report_cfg, &format);
            TableRendering {
                console: legacy.clone(),
                markdown: legacy,
            }
        };
        if rendered.console.is_empty() {
            eprintln!(
                "nmbrs summary: '{name}' produced no rows \
                       (db='{}').",
                db_path.display()
            );
            continue;
        }
        any_nonempty = true;
        let output_path = if !multiple && opts.output.is_some() {
            resolve_output_path(opts.output.as_deref(), &format, &output_anchor)
        } else {
            default_output_path(&basename, &format, &output_anchor)
        };
        if to_session {
            if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                eprintln!(
                    "nmbrs summary: failed to create output dir '{}': {e}",
                    parent.display()
                );
                std::process::exit(1);
            }
            // The artifact gets the GFM form — one header row, `<br>`-wrapped
            // headings — so it stays a real table when rendered. No fence needed.
            if let Err(e) = std::fs::write(&output_path, &rendered.markdown) {
                eprintln!(
                    "nmbrs summary: failed to write '{}': {e}",
                    output_path.display()
                );
                std::process::exit(1);
            }
            eprintln!("summary: {}", output_path.display());
        }

        // Upsert into the framing markdown report (default
        // `<db_dir>/summary.md`). Only Markdown summaries embed
        // inline; CSV/other formats record a link to the file
        // since rendering them inline would be unreadable.
        if to_session && !opts.report_disabled {
            let report_path = opts.report.clone().unwrap_or_else(|| {
                let dir = output_anchor
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                dir.join("summary.md")
            });
            // Don't recursively upsert into the same file we're
            // rendering when --output happens to be summary.md.
            if report_path != output_path {
                let body = if format == "md" {
                    // Embedded in a markdown document, so the GFM form.
                    rendered.markdown.clone()
                } else {
                    let leaf = output_path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| output_path.to_string_lossy().into_owned());
                    format!("[{leaf}]({leaf})\n")
                };
                let label = opts
                    .label
                    .clone()
                    .unwrap_or_else(|| crate::report::prettify_name(&basename));
                let heading_display = match opts.figure_num {
                    Some(n) => format!("{n}. {label} (table)"),
                    None => format!("{label} (table)"),
                };
                let mode = opts.report_mode.unwrap_or(crate::report::WriteMode::Update);
                match crate::report::write_named_section(
                    &report_path,
                    &basename,
                    &heading_display,
                    &body,
                    mode,
                ) {
                    Ok(true) => {}
                    Ok(false) => eprintln!(
                        "report: {} (skipped — section exists, --add-to-markdown mode)",
                        report_path.display()
                    ),
                    Err(e) => eprintln!(
                        "warning: failed to update report '{}': {e}",
                        report_path.display()
                    ),
                }
            }
        }

        // Console form, on whichever stream the routing named.
        // With multiple reports, prefix each with a separator
        // banner so a piped consumer can distinguish them.
        if to_stdout {
            if multiple {
                println!("=== {name} → {} ===", output_path.display());
            }
            print!("{}", rendered.console);
        }
        if to_stderr {
            if multiple {
                eprintln!("=== {name} → {} ===", output_path.display());
            }
            eprint!("{}", rendered.console);
        }
    }
    if !any_nonempty && !opts.empty_ok {
        std::process::exit(1);
    }
}

/// Default output path for a single summary: live in the db's
/// session directory, named `<basename>_summary.<format>`.
fn default_output_path(basename: &str, format: &str, db_path: &Path) -> PathBuf {
    let dir = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!("{basename}_summary.{format}"))
}

/// Wrap a stored hit into the `to_render` shape.
fn return_stored_or_literal(name: &str, raw: &str) -> Vec<(String, SummaryConfig)> {
    vec![(name.to_string(), SummaryConfig::parse(raw))]
}

/// Wrap an ad-hoc literal spec into the `to_render` shape, using
/// `override_name` if supplied (currently unused — the new CLI
/// reserves names for stored entries) else `"default"`.
fn literal_spec(spec: &str, override_name: Option<&str>) -> Vec<(String, SummaryConfig)> {
    let name = override_name.unwrap_or("default").to_string();
    vec![(name, SummaryConfig::parse(spec))]
}

/// Resolve the output path:
/// - `--output <path>`: as-is if it has any extension, otherwise
///   append `.{format}`.
/// - no `--output`: same directory as the metrics db, basename
///   `summary.{format}`.
fn resolve_output_path(
    user_output: Option<&str>,
    format: &str,
    db_path: &std::path::Path,
) -> PathBuf {
    match user_output {
        Some(path) => {
            let p = PathBuf::from(path);
            if p.extension().is_none() {
                let mut q = p;
                q.set_extension(format);
                q
            } else {
                p
            }
        }
        None => {
            let dir = db_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            dir.join(format!("summary.{format}"))
        }
    }
}

#[derive(Default)]
struct SummaryOpts {
    /// Bare positional. With `--create`, this is the spec to
    /// persist. Without `--name`, this is the ad-hoc literal
    /// spec to render. With `--name` and no `--create`, this
    /// must be empty (the user is referring to a stored name).
    spec: Option<String>,
    /// Reference to a stored named summary
    /// (`--name <NAME>`). Without `--create`, looks up the
    /// stored entry — error if missing. With `--create`,
    /// names the new entry being persisted from `spec`.
    name: Option<String>,
    /// Persist a new named summary (`--create`). Requires
    /// both `--name <NAME>` and a positional `<spec>`.
    create: bool,
    /// First-given db; preserved separately so the existing
    /// "single-db" diagnostic and default-output-path code
    /// works unchanged.
    db: Option<PathBuf>,
    /// Every db given via `--db` (repeatable, or
    /// comma-separated). When more than one is present, the
    /// summary command merges them into a temp db first, then
    /// runs SqliteReporter against the merged file. Sessions
    /// of the same workload are deduplicated by stripping the
    /// `session=` label so summary aggregates flow across all
    /// inputs as if from one logical session.
    dbs: Vec<PathBuf>,
    format: Option<String>,
    output: Option<String>,
    /// Source named summaries from a workload YAML's
    /// `summary:` block instead of the metrics db's
    /// `session_metadata`. Useful before any run has happened,
    /// or to overlay a fresh spec set on existing data.
    workload: Option<PathBuf>,
    /// Path to the framing markdown report. Each rendered
    /// summary is upserted as a `## summary: <name>` section.
    /// Default: `<db_dir>/summary.md`. `--report=skip` to
    /// suppress.
    report: Option<PathBuf>,
    /// Collision policy. `--update-markdown` (default) replaces
    /// existing same-anchor sections in place; `--add-to-markdown`
    /// only appends when no section under the same anchor
    /// exists, leaving existing content untouched.
    report_mode: Option<crate::report::WriteMode>,
    /// True when `--report=skip` / `--no-report` is passed.
    report_disabled: bool,
    /// SRD-46: figure number injected by `nmbrs report` for
    /// markdown heading prefix.
    figure_num: Option<usize>,
    /// SRD-46: display label injected by `nmbrs report`. Falls
    /// back to a prettified name.
    label: Option<String>,
    /// `--empty-ok` — an all-empty render returns instead of
    /// `exit(1)`. Injected by the `nmbrs report` pipeline (and the
    /// post-run auto-render): a report table whose phases did not
    /// run this session legitimately has no rows, and one empty
    /// table must not abort the remaining items — or the process.
    empty_ok: bool,
    /// SRD-46 `--to` — where this render is delivered. `None` ⇒
    /// the historical default (session dir + stdout), which is
    /// what a hand-typed `nmbrs summary` should do. The `nmbrs
    /// report` pipeline forwards the workload's declared routing
    /// here for the automatic end-of-run render, where stdout is
    /// NOT part of the default.
    to: Option<Vec<nmbrs_workload::report::Destination>>,
}

impl SummaryOpts {
    /// The effective destination set: the declared routing, or
    /// the hand-typed default.
    fn destinations(&self) -> Vec<nmbrs_workload::report::Destination> {
        use nmbrs_workload::report::Destination as D;
        self.to
            .clone()
            .unwrap_or_else(|| vec![D::SessionDir, D::Stdout])
    }
}

/// Whether a token is a `key=value` run/read param (`session=…`, `phases=…`,
/// `cycles=…`) rather than a summary spec.
///
/// These reach the renderer because callers forward their whole argument tail.
/// Without this test the first one became `opts.spec` — an ad-hoc spec whose
/// directives are all unrecognised, which renders the DEFAULT summary table
/// under the requested item's name and file. `nmbrs table compaction_shape
/// session=<dir>` produced a generic activity table titled "Summary" and
/// reported success. A key=value token is never a spec: a spec body starts with
/// a directive keyword, and `key=value` with no space is the param shape.
fn is_kv_param_token(tok: &str) -> bool {
    let Some((key, _)) = tok.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && !key.contains(char::is_whitespace)
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn parse_args(args: &[String]) -> SummaryOpts {
    let mut opts = SummaryOpts::default();
    // `--session` / `--session-path` / `--session-name` resolve
    // to a session dir uniformly across read-side tools — see
    // `nmbrs_runtime::session::read_session_dir`. `--db` below
    // overrides this when it's given explicitly.
    if let Some(session_dir) = nmbrs_runtime::session::read_session_dir(args) {
        opts.db = Some(session_dir.join("metrics.db"));
    }
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--db" => {
                if let Some(v) = iter.next() {
                    for path in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        opts.dbs.push(PathBuf::from(path));
                    }
                    if opts.db.is_none() {
                        opts.db = opts.dbs.first().cloned();
                    }
                }
            }
            "--format" => {
                if let Some(v) = iter.next() {
                    opts.format = Some(v.clone());
                }
            }
            "--output" => {
                if let Some(v) = iter.next() {
                    opts.output = Some(v.clone());
                }
            }
            "--name" => {
                if let Some(v) = iter.next() {
                    opts.name = Some(v.clone());
                }
            }
            "--label" => {
                if let Some(v) = iter.next() {
                    opts.label = Some(v.clone());
                }
            }
            // SRD-46 output routing. Absent ⇒ the historical
            // default (session dir + stdout), which is what an
            // operator typing `nmbrs summary` expects to see.
            "--to" => {
                if let Some(v) = iter.next() {
                    match nmbrs_workload::report::Destination::parse_list(v) {
                        Ok(d) => opts.to = Some(d),
                        Err(e) => {
                            eprintln!("nmbrs summary: --to: {e}");
                            std::process::exit(2);
                        }
                    }
                }
            }
            "--figure-num" => {
                if let Some(v) = iter.next()
                    && let Ok(n) = v.parse::<usize>()
                {
                    opts.figure_num = Some(n);
                }
            }
            "--create" => {
                opts.create = true;
            }
            "--report" | "--update-markdown" => {
                if let Some(v) = iter.next() {
                    if v == "skip" || v.is_empty() {
                        opts.report_disabled = true;
                    } else {
                        opts.report = Some(PathBuf::from(v));
                        opts.report_mode = Some(crate::report::WriteMode::Update);
                    }
                }
            }
            "--add-to-markdown" => {
                if let Some(v) = iter.next() {
                    opts.report = Some(PathBuf::from(v));
                    opts.report_mode = Some(crate::report::WriteMode::AddIfMissing);
                }
            }
            "--no-report" => opts.report_disabled = true,
            "--empty-ok" => opts.empty_ok = true,
            // Global session flags — already consumed by
            // `read_session_dir` above. Swallow the value so
            // it doesn't drift into `opts.spec` as a stray
            // positional.
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife"
            | "--resume"
            | "--polydat-lib" => {
                let _ = iter.next();
            }
            "--strict" | "--no-prompt" | "--resume-latest" | "--force-retry-failed" => {}
            other => {
                if let Some(v) = other.strip_prefix("--db=") {
                    for path in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        opts.dbs.push(PathBuf::from(path));
                    }
                    if opts.db.is_none() {
                        opts.db = opts.dbs.first().cloned();
                    }
                } else if let Some(v) = other.strip_prefix("--format=") {
                    opts.format = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--output=") {
                    opts.output = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--name=") {
                    opts.name = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("workload=") {
                    let resolved =
                        crate::cli::resolve_workload_path(v).unwrap_or_else(|| v.to_string());
                    opts.workload = Some(PathBuf::from(resolved));
                } else if let Some(v) = other
                    .strip_prefix("--report=")
                    .or_else(|| other.strip_prefix("--update-markdown="))
                {
                    if v == "skip" || v.is_empty() {
                        opts.report_disabled = true;
                    } else {
                        opts.report = Some(PathBuf::from(v));
                        opts.report_mode = Some(crate::report::WriteMode::Update);
                    }
                } else if let Some(v) = other.strip_prefix("--add-to-markdown=") {
                    opts.report = Some(PathBuf::from(v));
                    opts.report_mode = Some(crate::report::WriteMode::AddIfMissing);
                } else if !other.starts_with("--")
                    && !is_kv_param_token(other)
                    && opts.spec.is_none()
                {
                    // First bare positional is the spec / stored
                    // name / `*` shortcut. Subsequent positionals
                    // are silently ignored; the previous CLI
                    // shape only ever accepted one.
                    opts.spec = Some(other.to_string());
                }
            }
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn bare_yields_no_spec() {
        let opts = parse_args(&[]);
        assert!(
            opts.spec.is_none(),
            "bare `summary` should leave spec as None (lists stored)"
        );
    }

    #[test]
    fn first_positional_becomes_spec() {
        let opts = parse_args(&[s("recall; mean(recall) over profile~label")]);
        assert_eq!(
            opts.spec.as_deref(),
            Some("recall; mean(recall) over profile~label")
        );
    }

    /// One magnitude per column, chosen from that column's largest value, so rows
    /// stay comparable without counting digits.
    #[test]
    fn si_scale_is_per_column_not_per_value() {
        assert_eq!(SiScale::for_max(19_594_373_133.0).unwrap().symbol, "G");
        assert_eq!(SiScale::for_max(489_170_772.0).unwrap().symbol, "M");
        assert_eq!(SiScale::for_max(23_187.0).unwrap().symbol, "K");
        // Small enough to read as-is — a suffix would only add a decimal point.
        assert!(SiScale::for_max(999.0).is_none());
        assert!(SiScale::for_max(0.0).is_none());
        // A column's smallest values ride the column's scale, not their own:
        // 489170772 in a column topping 19.6G shows as 0.49, not 489.17M.
        let col = SiScale::for_max(19_594_373_133.0).unwrap();
        assert_eq!(format!("{:.2}", 489_170_772.0 / col.divisor), "0.49");
    }

    /// A value already in seconds needs a time scale, not an SI one: the duration
    /// machinery elsewhere assumes nanoseconds, and SI alone renders a compaction
    /// that has run 2000 seconds as "2.00 (K)" — kiloseconds.
    #[test]
    fn seconds_domain_columns_scale_as_time() {
        // A clock, not a scaled number: "4.50 (min)" makes the reader convert, and
        // a magnitude that shifts with the column hides that 0.50 and 4.50 are
        // 30 seconds and 4 and a half minutes.
        assert_eq!(format_hms(7_200.0), "02:00:00");
        assert_eq!(format_hms(2_080.0), "00:34:40");
        assert_eq!(format_hms(45.0), "00:00:45");
        assert_eq!(format_hms(0.0), "00:00:00");
        assert_eq!(format_hms(f64::NAN), "-");

        // A difference of two moments IS a duration.
        assert!(is_seconds_domain_query(
            "max(tlast_over_time(x[7d])) - on() group_right() min(tfirst_over_time(y[7d])) by (p)"
        ));
        // A rate that divides BY that difference is not: it keeps its numerator's
        // units, and calling it "(h)" turned bytes-per-ms into hours.
        assert!(!is_seconds_domain_query(
            "max(bytes) by (p) / (1000 * (max(tlast_over_time(x[7d])) - on() group_right() min(tfirst_over_time(y[7d])) by (p)))"
        ));
        // A bare moment is neither — it is an instant, handled as a timestamp.
        assert!(!is_seconds_domain_query(
            "min(tfirst_over_time(x[7d])) by (p)"
        ));
        assert!(is_timestamp_query("min(tfirst_over_time(x[7d])) by (p)"));
    }

    #[test]
    fn kv_param_tokens_do_not_become_spec() {
        // Callers forward their whole argument tail, so run/read params land
        // here. Taken as a spec, `session=<dir>` renders the DEFAULT summary
        // table under the requested item's name and reports success — a wrong
        // table that looks right. `nmbrs table compaction_shape session=<dir>`
        // did exactly that.
        for tok in ["session=/tmp/s", "phases=load", "cycles=1..10", "tries=3"] {
            let opts = parse_args(&[s(tok)]);
            assert!(
                opts.spec.is_none(),
                "`{tok}` is a param, not a spec (got {:?})",
                opts.spec
            );
        }
    }

    #[test]
    fn spec_with_an_equals_sign_still_parses_as_a_spec() {
        // A spec body contains `=` inside filters, so the param test must key
        // on the `key=` SHAPE (no whitespace before `=`), not on `=` presence.
        let spec = "recall; max(recall{phase=\"ann\"}) over profile";
        let opts = parse_args(&[s(spec)]);
        assert_eq!(opts.spec.as_deref(), Some(spec));
        let multi = "group_by: p\nquery: segments: max(compaction_input_segments) by (p)";
        assert_eq!(parse_args(&[s(multi)]).spec.as_deref(), Some(multi));
    }

    #[test]
    fn flags_do_not_become_spec() {
        let opts = parse_args(&[s("--db"), s("/tmp/m.db")]);
        assert!(
            opts.spec.is_none(),
            "flags must not be parsed as the spec positional"
        );
        assert_eq!(opts.db.as_deref(), Some(std::path::Path::new("/tmp/m.db")));
    }

    #[test]
    fn output_extension_added_when_basename_only() {
        let p = resolve_output_path(Some("report"), "md", std::path::Path::new("/tmp/m.db"));
        assert_eq!(p, PathBuf::from("report.md"));
    }

    #[test]
    fn output_extension_preserved_when_present() {
        let p = resolve_output_path(Some("/tmp/x.csv"), "md", std::path::Path::new("/tmp/m.db"));
        assert_eq!(
            p,
            PathBuf::from("/tmp/x.csv"),
            "explicit extension wins over --format default"
        );
    }

    #[test]
    fn default_output_lives_alongside_db() {
        let p = resolve_output_path(
            None,
            "md",
            std::path::Path::new("logs/session_1/metrics.db"),
        );
        assert_eq!(p, PathBuf::from("logs/session_1/summary.md"));
    }

    #[test]
    fn all_options_combined() {
        let opts = parse_args(&[
            s("recall"),
            s("--db"),
            s("/tmp/m.db"),
            s("--format"),
            s("md"),
            s("--output"),
            s("/tmp/out"),
        ]);
        assert_eq!(opts.spec.as_deref(), Some("recall"));
        assert_eq!(opts.db.as_deref(), Some(std::path::Path::new("/tmp/m.db")));
        assert_eq!(opts.format.as_deref(), Some("md"));
        assert_eq!(opts.output.as_deref(), Some("/tmp/out"));
    }

    #[test]
    fn name_alone_targets_stored() {
        let opts = parse_args(&[s("--name"), s("recall_v1")]);
        assert_eq!(opts.name.as_deref(), Some("recall_v1"));
        assert!(!opts.create);
        assert!(opts.spec.is_none());
    }

    #[test]
    fn name_with_create_and_spec() {
        let opts = parse_args(&[
            s("--name"),
            s("recall_v1"),
            s("--create"),
            s("recall; mean(recall)"),
        ]);
        assert_eq!(opts.name.as_deref(), Some("recall_v1"));
        assert!(opts.create);
        assert_eq!(opts.spec.as_deref(), Some("recall; mean(recall)"));
    }
}

#[cfg(test)]
mod natural_sort_tests {
    use super::{natural_cmp_one, natural_cmp_pipe_tuple};
    use std::cmp::Ordering;

    #[test]
    fn embedded_numbers_order_by_magnitude() {
        // The regression: report rows came out 0, 1, 10, 11, ... 2, 20 because a
        // label with a number inside text fell back to lexical comparison.
        let mut keys = vec![
            "Partition(10/36 [1000000..2000000))",
            "Partition(2/36 [200000..300000))",
            "Partition(0/36 [0..100000))",
            "Partition(20/36 [20000000..30000000))",
            "Partition(1/36 [100000..200000))",
        ];
        keys.sort_by(|a, b| natural_cmp_one(a, b));
        assert_eq!(
            keys,
            vec![
                "Partition(0/36 [0..100000))",
                "Partition(1/36 [100000..200000))",
                "Partition(2/36 [200000..300000))",
                "Partition(10/36 [1000000..2000000))",
                "Partition(20/36 [20000000..30000000))",
            ]
        );
    }

    #[test]
    fn sweep_labels_order_by_magnitude() {
        let mut keys = vec!["k=10", "k=2", "k=100", "k=1"];
        keys.sort_by(|a, b| natural_cmp_one(a, b));
        assert_eq!(keys, vec!["k=1", "k=2", "k=10", "k=100"]);
    }

    #[test]
    fn wholly_numeric_tokens_keep_float_semantics() {
        // Segment-wise comparison alone would read "1.5" as 1 then 5 and place it
        // after "10"; the whole-token fast path keeps decimals correct.
        assert_eq!(natural_cmp_one("1.5", "10"), Ordering::Less);
        assert_eq!(natural_cmp_one("2", "10"), Ordering::Less);
        assert_eq!(natural_cmp_one("10", "10"), Ordering::Equal);
    }

    #[test]
    fn leading_zeros_do_not_change_order() {
        assert_eq!(natural_cmp_one("tier007", "tier7"), Ordering::Equal);
        assert_eq!(natural_cmp_one("tier007", "tier8"), Ordering::Less);
    }

    #[test]
    fn long_digit_runs_do_not_overflow() {
        // Compared by significant length then bytes, never parsed, so runs far
        // beyond u64 still order correctly.
        let big = format!("x{}", "9".repeat(40));
        let bigger = format!("x1{}", "0".repeat(40));
        assert_eq!(natural_cmp_one(&big, &bigger), Ordering::Less);
    }

    #[test]
    fn shorter_prefix_sorts_first() {
        assert_eq!(natural_cmp_one("tier2", "tier2a"), Ordering::Less);
    }

    #[test]
    fn pipe_tuples_compare_segment_by_segment() {
        // Multi-key group_by joins label values with `|`; each segment is natural.
        assert_eq!(natural_cmp_pipe_tuple("a|2", "a|10"), Ordering::Less);
        assert_eq!(
            natural_cmp_pipe_tuple("tier2|x", "tier10|a"),
            Ordering::Less
        );
    }
}
