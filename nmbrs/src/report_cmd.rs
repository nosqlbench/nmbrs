// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs report` — list and render report items defined in a
//! workload's `report:` block (SRD-46).
//!
//! Subcommands:
//!
//! - `nmbrs report` (no args) — list every defined item.
//! - `nmbrs report all` — render every item.
//! - `nmbrs report <glob>` — render items whose names match the
//!   glob.
//! - `nmbrs report figure <N>` — render by global index.
//! - `nmbrs report plot <glob>` / `nmbrs report table <glob>` —
//!   kind-filtered name lookup.
//!
//! All forms accept `workload=<file>` positionally, falling back
//! to `logs/latest/metrics.db`'s persisted items when no source
//! is given.

use std::path::{Path, PathBuf};

/// Top-level dispatch for `nmbrs report ...` and the unadvertised
/// `nmbrs plot ...` / `nmbrs table ...` aliases.
///
/// The `workload=<file>` token may appear anywhere in the args
/// list — it's pulled out for source resolution, then re-injected
/// when forwarding render commands to plot_metrics / summary.
pub fn report_command(args: &[String], kind_filter: KindFilter) {
    let (mut workload_path, rest) = extract_workload(args);
    // SRD-109 — `--synthesized`: resolve items from the SYNTHESIZED
    // section irrespective of any explicit `report:` block. The
    // affine mirror is always renderable, not just dumpable. Exact
    // spelling only — near-misses are rejected by the kindless
    // flag-form path's closed-surface check with a "did you mean",
    // never silently accepted or silently forwarded.
    let synthesized_only = rest.iter().any(|a| a == "--synthesized");
    let rest: Vec<String> = rest.into_iter().filter(|a| a != "--synthesized").collect();
    // Resolve `--session` once at the top so every downstream
    // path (item lookup in db, forwarded render commands,
    // markdown output, text-section writes) sees the same
    // session dir. Read-side only — never mutates `logs/latest`.
    let flagged_session: Option<PathBuf> = nmbrs_runtime::session::read_session_dir(args);
    // An explicit `--db <path>` names a session as surely as `--session <dir>`
    // does — the db's directory IS the session. Considering only `--session*`
    // before, a `--db` invocation resolved items and wrote output under
    // `sessions/latest` while the renderer read data from the named db: two
    // different sessions in one command, with the item list from the wrong one.
    // `--session` still wins when both appear, being the more explicit intent.
    let flagged_db: Option<PathBuf> = db_flag_path(args);
    // The db path is carried, not rebuilt from the session dir: a `--db` need
    // not be named `metrics.db`, and rebuilding would point at a sibling that
    // may not exist.
    let session_db: Option<PathBuf> = match (&flagged_session, &flagged_db) {
        (Some(dir), _) => Some(dir.join("metrics.db")),
        (None, Some(db)) => Some(db.clone()),
        (None, None) => None,
    };
    let session_dir: Option<PathBuf> = flagged_session.or_else(|| {
        flagged_db
            .as_deref()
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
    });
    let output_root: PathBuf = session_dir
        .clone()
        .unwrap_or_else(nmbrs_runtime::session::latest_session_dir);
    // The SESSION root, kept separate from the render output root
    // below. Rendered report artifacts may be redirected into
    // `<session>/report/`, but session-local machinery — the
    // `scratch/` area, `metrics.db`, workload resolution for
    // `--add` / `rename` — is anchored to the session itself and
    // must not follow that redirect.
    let session_root: PathBuf = output_root.clone();

    // `--synthesized` synthesizes from workload FIXTURES, which live
    // in the yaml — session-db report.* rows can't provide them.
    // Without an explicit `workload=`, resolve the yaml the session
    // itself records (`workload_file` execution metadata) rather than
    // silently falling back to the db rows the flag exists to bypass.
    if synthesized_only && workload_path.is_none() {
        let db_path = session_db
            .clone()
            .unwrap_or_else(|| output_root.join("metrics.db"));
        match workload_recorded_in(&db_path) {
            Some(p) if p.exists() => {
                eprintln!(
                    "nmbrs report: --synthesized — using the session's \
                    recorded workload ({})",
                    p.display()
                );
                workload_path = Some(p);
            }
            Some(p) => {
                eprintln!(
                    "nmbrs report: --synthesized needs the workload yaml \
                    (the session records `{}`, which does not exist from this \
                    directory). Pass workload=<file>.",
                    p.display()
                );
                std::process::exit(2);
            }
            None => {
                eprintln!(
                    "nmbrs report: --synthesized needs the workload yaml \
                    and the session db ({}) records no workload_file. \
                    Pass workload=<file>.",
                    db_path.display()
                );
                std::process::exit(2);
            }
        }
    }
    let workload_arg = workload_path
        .as_ref()
        .map(|p| format!("workload={}", p.display()));

    // Promote `nmbrs report plot ...` / `nmbrs report table ...` to
    // the kind-filtered form, peeling the kind keyword off so the
    // remaining arg list looks like a top-level `nmbrs plot ...`
    // / `nmbrs table ...` invocation.
    let (kind_filter, rest) = if matches!(kind_filter, KindFilter::Any) {
        match rest.first().map(String::as_str) {
            Some("plot") => (KindFilter::Plot, rest[1..].to_vec()),
            Some("table") => (KindFilter::Table, rest[1..].to_vec()),
            _ => (kind_filter, rest),
        }
    } else {
        (kind_filter, rest)
    };

    // Kindless with a leading `--token`: that shape routes to the
    // flag-form arm, whose classification is only honest for flags
    // the surface actually has. Check the closed set NOW — before
    // resolving items and printing the preamble — so a typo'd flag
    // is the first and only thing reported.
    if matches!(kind_filter, KindFilter::Any)
        && let Some(first) = rest.first().filter(|a| a.starts_with("--"))
    {
        reject_if_unknown_flag(first);
    }

    // Artifact accounting: snapshot the output dir before dispatch so
    // the closing summary can name exactly the files this invocation
    // created or updated — regardless of which renderer wrote them.
    let (mut items, items_synthesized) = match resolve_items(
        workload_path.as_deref(),
        session_db.as_deref(),
        synthesized_only,
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nmbrs report: {e}");
            std::process::exit(2);
        }
    };

    // SRD-46 routing — EXPLICIT invocation. `to:` governs the
    // automatic end-of-run render; typing `nmbrs report` is a
    // request to SEE the report, so the declaration is dropped
    // here and the renderer's own default (session dir + stdout)
    // applies. An operator who wants something else says so with
    // `--to`, which reaches the renderer through `passthrough`.
    // Without this, a workload declaring `to: sessiondir` would
    // make an explicit report command print nothing at all.
    for it in &mut items {
        it.destinations = None;
    }
    let items = items;

    // Synthesized reports land in `<session>/report/` — disentangled
    // from tables and plots earlier hand-written renders left in the
    // session root. Explicit `report:` blocks keep their historical
    // root placement.
    let output_root: PathBuf = if items_synthesized {
        let dir = output_root.join("report");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("nmbrs report: cannot create {}: {e}", dir.display());
            std::process::exit(2);
        }
        dir
    } else {
        output_root
    };

    let artifacts_before = artifact_snapshot(&output_root);

    // Operator-visible summary of what's about to happen.
    // The prior silent zero-items behaviour was the
    // load-bearing UX bug — `nmbrs report all` against a
    // live session (whose sqlite has yet to flush its
    // report-items rows) produced no output AND no
    // diagnostic. Surface every relevant input now so the
    // operator can correct course without guessing.
    // (`report synth` is a pure stdout dump — it renders
    // nothing and touches no session dir, so the resolved/
    // output preamble would only mislead.)
    let is_synth_dump = rest.first().map(String::as_str) == Some("synth");
    let source_kind = if workload_path.is_some() {
        "workload yaml"
    } else {
        "session db (report.* metadata rows)"
    };
    let source_path: String = workload_path
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            session_db
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| {
                    nmbrs_runtime::session::latest_metrics_db()
                        .display()
                        .to_string()
                })
        });
    if !is_synth_dump {
        eprintln!(
            "nmbrs report: {} item(s) resolved from {} ({})",
            items.len(),
            source_kind,
            source_path
        );
        eprintln!("nmbrs report: output → {}", output_root.display());
    }
    if items.is_empty() {
        // The two paths that yield zero items are
        // distinguishable; flag the most common one so the
        // operator can act.
        if workload_path.is_none() {
            eprintln!(
                "nmbrs report: hint — no report.* rows in \
                the session db yet. For a LIVE session (sqlite \
                flushes on its own cadence — see SRD-40b), pass \
                `workload=<file>` to resolve items from the \
                YAML source instead. Example: \
                `nmbrs report all workload=adapters/<adapter>/workloads/<workload>.yaml`"
            );
        } else {
            eprintln!(
                "nmbrs report: hint — the workload yaml \
                has no `report:` items declared. Add a \
                `report:` block to surface plots / tables."
            );
        }
    }

    // `--rebuild` (or `NMBRS_REPORT_REBUILD=1`) wipes the
    // declared target markdown files before any renderer
    // touches them. The set comes from the resolved item
    // list — files outside the declared targets stay
    // intact. Idempotent: missing files are silently
    // skipped (a fresh session has nothing to wipe).
    if is_rebuild_mode(args) {
        rebuild_wipe_targets(&items, &output_root);
    }

    // `--clean` (only honored with the `all` target) wipes
    // every `.png` and `.md` file in the session output
    // directory before rendering. Use when the workload's
    // `report:` block has changed (items renamed, removed,
    // or relettered) and you want the resulting directory
    // to exactly reflect the current declaration set —
    // including no leftover artifact files from prior runs.
    // Stronger than `--rebuild`: that one only deletes the
    // markdown report files (`target_file` paths); this one
    // also sweeps the individual plot PNGs and companion-
    // table MDs.
    if is_clean_mode(args) && rest.first().map(String::as_str) == Some("all") {
        clean_wipe_artifacts(&output_root);
    }

    // SRD-15 strict mode: when `--strict` is on the arg list
    // (or `NMBRS_STRICT` is set), figure-render no-data errors
    // remain hard failures. Without strict mode, "no-data"
    // results from incremental / auto-render paths are
    // downgraded to warnings so a workload reporting before
    // its data has accumulated doesn't fail the run.
    let strict = is_strict_mode(args);

    let failures: Vec<String> = match rest.first().map(String::as_str) {
        // Listing form — no selector after the (optional) kind
        // keyword. `list` is an explicit alias for the bare
        // form: `nmbrs report list` and `nmbrs report` produce
        // the same output.
        None | Some("list") => {
            print_listing(&items, kind_filter);
            Vec::new()
        }
        Some("all") => render_all(
            &items,
            kind_filter,
            &rest[1..],
            workload_arg.as_deref(),
            &output_root,
            session_db.as_deref(),
            strict,
        ),
        Some("figure") => {
            let n_arg = rest.get(1).cloned().unwrap_or_default();
            let pass = rest.get(2..).unwrap_or(&[]);
            render_by_index(
                &items,
                kind_filter,
                &n_arg,
                pass,
                workload_arg.as_deref(),
                &output_root,
                session_db.as_deref(),
                strict,
            )
        }
        Some("scratch") => {
            crate::report_scratch::scratch_subcommand(&session_root, &rest[1..]);
            Vec::new()
        }
        // Declared subcommand `show`: render one stored item by name.
        // (Previously fell through to the glob arm carrying the literal
        // token "show", which matched nothing.)
        Some("show") => match rest.get(1).cloned() {
            Some(name) => render_by_glob(
                &items,
                kind_filter,
                &name,
                &rest[2..],
                workload_arg.as_deref(),
                &output_root,
                session_db.as_deref(),
                strict,
            ),
            None => {
                eprintln!("nmbrs report show: needs an item name");
                std::process::exit(2);
            }
        },
        Some("rename") => {
            run_rename(&rest[1..], &session_root, workload_path.as_deref());
            Vec::new()
        }
        // SRD-109 — dump the synthesized report section as a
        // `report:` YAML block: the affine round-trip surface. Copy
        // it into the workload and edit to hand-tune. An explicit
        // block suppresses only the IMPLICIT render path — this dump
        // (and `--synthesized` rendering) always reflect the fixtures.
        Some("synth") => {
            match workload_path.as_deref() {
                None => eprintln!("nmbrs report synth: needs --workload <file>"),
                Some(p) => {
                    match nmbrs_workload::parse::parse_workload_from_path(
                        p,
                        &std::collections::HashMap::new(),
                    )
                    .and_then(|w| nmbrs_workload::report_synth::synthesize_yaml(&w))
                    {
                        // Forced by design: the affine mirror dumps even when
                        // an explicit report: block exists (which suppresses
                        // only the IMPLICIT render path, not inspection).
                        Ok(yaml) => print!("{yaml}"),
                        Err(e) => eprintln!("nmbrs report synth: {e}"),
                    }
                }
            }
            Vec::new()
        }
        // Flag-form: `nmbrs plot --name X --series Y ...` — the
        // user is driving the renderer directly with its own
        // flags. Pass the whole arg list straight through
        // without trying to interpret any token as a glob.
        // Equivalent to the positional form for stored-name
        // selection (`nmbrs plot X`) but lets the user supply
        // ad-hoc `--metric`/`--filter`/etc.
        Some(arg) if arg.starts_with("--") => {
            forward_renderer_flags(
                kind_filter,
                &rest,
                workload_arg.as_deref(),
                session_db.as_deref(),
            );
            Vec::new()
        }
        // SRD-64 flag-form: `nmbrs report <kind> <name> [--<flag> ...]`.
        // When a vocab-defined `--flag` appears anywhere in the
        // argument tail, the user is constructing a new item from
        // CLI flags rather than selecting an existing stored one.
        // Route through the Phase A vocab-driven builder + Phase C
        // scratch render path.
        Some(_name)
            if matches!(kind_filter, KindFilter::Plot | KindFilter::Table)
                && tail_has_vocab_flag(&rest) =>
        {
            let kind = match kind_filter {
                KindFilter::Plot => nmbrs_workload::report::Kind::Plot,
                KindFilter::Table => nmbrs_workload::report::Kind::Table,
                _ => unreachable!(),
            };
            dispatch_new_item(kind, &rest, &session_root, workload_path.as_deref());
            Vec::new()
        }
        Some(arg) => {
            // Numeric-selector forms (`5`, `2-4`, `2..4`,
            // `2..=4`, `1,3-5,7`) route through the figure
            // index path. Item names follow the OpenMetrics
            // metric-name ABNF which forbids hyphens / dots /
            // commas, so a bare `2-4` can't be a literal item
            // name — safe to reinterpret as a figure
            // selector.
            if let Some(indices) = parse_figure_selector(arg) {
                render_by_indices(
                    &items,
                    kind_filter,
                    &indices,
                    &rest[1..],
                    workload_arg.as_deref(),
                    &output_root,
                    session_db.as_deref(),
                    strict,
                )
            } else {
                render_by_glob(
                    &items,
                    kind_filter,
                    arg,
                    &rest[1..],
                    workload_arg.as_deref(),
                    &output_root,
                    session_db.as_deref(),
                    strict,
                )
            }
        }
    };

    // Closing file inventory: everything new or modified under the
    // output root since dispatch (index.md excluded — it is rewritten
    // below on every invocation and would always appear).
    let written = artifacts_written_since(&output_root, &artifacts_before);
    if !written.is_empty() {
        let listing = written.join(", ");
        let prefix = format!(
            "nmbrs report: wrote {} file(s) under {}: ",
            written.len(),
            output_root.display()
        );
        eprintln!("{prefix}{}", wrap_hint(&listing, prefix.chars().count()));
    }

    // Refresh the session directory's `index.md` after every
    // `nmbrs report` invocation — even on partial-failure runs.
    // The index is a flat catalog of every artifact in the
    // directory (markdown reports, CSV data, plot images,
    // tables, JSON, logs, etc.), so operators can navigate the
    // session contents without `ls`. Best-effort: a write
    // failure logs at stderr but does not affect the
    // report-command exit code.
    if let Err(e) = refresh_session_index(&output_root) {
        eprintln!(
            "nmbrs report: failed to update {}: {e}",
            output_root.join("index.md").display()
        );
    }

    // SRD: figure-render failures are not skip-overable. The
    // workload defined a figure; the operator asked for it; the
    // run produced no output. That is a defect worth a nonzero
    // exit, even if the rest of the batch produced their
    // markdown/png artifacts. Each failure was already printed
    // line-by-line as `ERROR: ...` above; the trailing summary
    // gives the count and the exit-time signal.
    if !failures.is_empty() {
        eprintln!();
        eprintln!(
            "nmbrs report: {} figure(s) failed to render:",
            failures.len()
        );
        for f in &failures {
            eprintln!("  - {f}");
        }
        std::process::exit(2);
    }
}

/// Walk the session directory once and write a categorised
/// `index.md` that links to every artifact in it. Idempotent
/// (overwrites the prior index), top-level only (no recursion
/// into subdirectories), and best-effort (returns
/// [`std::io::Error`] on failure so the caller can warn).
///
/// Categorisation by extension:
/// - **Reports** (`.md` excluding `index.md` itself, `.txt`)
/// - **Tables / data** (`.csv`, `.tsv`, `.json`, `.jsonl`,
///   `.parquet`)
/// - **Figures** (`.png`, `.svg`, `.jpg`, `.jpeg`, `.gif`,
///   `.webp`, `.pdf`)
/// - **Logs** (`.log`)
/// - **Database** (`.db`, `.sqlite`, `.sqlite3`)
/// - **Other** — anything that didn't match. Always last; a
///   future artifact type lands here without breaking the
///   index.
///
/// Within each category, entries sort by filename for stable
/// diffs across invocations. Each entry renders as a markdown
/// link `[filename.ext](filename.ext)` so clicking from a
/// markdown preview opens the file relative to the index.
fn refresh_session_index(output_root: &Path) -> std::io::Result<()> {
    use std::fmt::Write as _;
    use std::io::Write as _;

    // Resolve through any symlinks once at the top. If
    // `output_root` is a broken / self-looping symlink (which
    // happens when an external process mismanages
    // `logs/latest`), `canonicalize` returns an error AND the
    // `read_dir` below would loop with `Too many levels of
    // symbolic links (os error 40)`. Treat the resolution
    // failure as a best-effort no-op — refusing to crash the
    // report-command path that's about to run against the
    // user's actual plot data. The error message identifies
    // the path so the operator can fix the symlink.
    let resolved = match std::fs::canonicalize(output_root) {
        Ok(p) => p,
        Err(e) => {
            return Err(std::io::Error::new(
                e.kind(),
                format!(
                    "cannot resolve session directory '{}': {e} \
                    (a broken or self-looping symlink? — index skipped)",
                    output_root.display()
                ),
            ));
        }
    };

    // Top-level scan only — subdirectories aren't part of the
    // report-artifact contract; their indexing is a future
    // concern if one ever lands.
    let mut entries: Vec<(String, String)> = Vec::new();
    let dir = std::fs::read_dir(&resolved)?;
    for entry in dir {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s.to_string(),
            None => continue, // skip non-UTF8 filenames
        };
        // Skip the index itself so a future re-run doesn't
        // self-list with a relative loop. Also skip dotfiles
        // and lockfiles — operator clutter.
        if name == "index.md" {
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        if name.ends_with(".lock") {
            continue;
        }
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let category = match ext.as_str() {
            "md" | "txt" => "reports",
            "csv" | "tsv" | "json" | "jsonl" | "parquet" => "data",
            "png" | "svg" | "jpg" | "jpeg" | "gif" | "webp" | "pdf" => "figures",
            "log" => "logs",
            "db" | "sqlite" | "sqlite3" | "shm" | "wal" => "database",
            _ => "other",
        };
        entries.push((category.to_string(), name));
    }
    // Stable order within each category for diff-friendly output.
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut by_category: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    // Preserve a fixed display order via this explicit list;
    // BTreeMap above guarantees stable iteration but in
    // alphabetical key order, which doesn't match the
    // doc-section narrative ("read the reports first, then
    // figures, then data, then logs, then everything else").
    let display_order = ["reports", "figures", "data", "logs", "database", "other"];
    for (cat, name) in &entries {
        by_category
            .entry(cat.as_str())
            .or_default()
            .push(name.as_str());
    }

    // Build the markdown body once, then write atomically via
    // a tmp file + rename so a concurrent reader never sees a
    // half-written index. (Same pattern the cadence sqlite
    // writer uses.)
    let mut body = String::new();
    let dir_name = output_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    writeln!(body, "# Index — `{dir_name}`").unwrap();
    writeln!(body).unwrap();
    writeln!(
        body,
        "Auto-generated by `nmbrs report`. Re-runs of \
        any `nmbrs report` subcommand refresh this index."
    )
    .unwrap();
    writeln!(body).unwrap();

    let mut wrote_anything = false;
    for cat in display_order {
        let Some(names) = by_category.get(cat) else {
            continue;
        };
        if names.is_empty() {
            continue;
        }
        let heading = match cat {
            "reports" => "Reports",
            "figures" => "Figures",
            "data" => "Tables / data",
            "logs" => "Logs",
            "database" => "Database",
            "other" => "Other",
            _ => cat,
        };
        writeln!(body, "## {heading}").unwrap();
        writeln!(body).unwrap();
        for name in names {
            // Inline figures (images) get an embedded preview
            // so the markdown renderer shows them in place; the
            // bracket form (without the leading `!`) is a plain
            // link used for everything else.
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            let is_image = matches!(
                ext.as_str(),
                "png" | "svg" | "jpg" | "jpeg" | "gif" | "webp"
            );
            if is_image {
                writeln!(body, "- [`{name}`]({name})  ").unwrap();
                writeln!(body, "  ![{name}]({name})").unwrap();
            } else {
                writeln!(body, "- [`{name}`]({name})").unwrap();
            }
        }
        writeln!(body).unwrap();
        wrote_anything = true;
    }
    if !wrote_anything {
        writeln!(body, "_(no artifacts in this session directory)_").unwrap();
    }

    // Write through the RESOLVED path so a symlink loop or
    // a moving `logs/latest` target can't redirect us
    // mid-write.
    let index_path = resolved.join("index.md");
    let tmp_path = resolved.join(".index.md.tmp");
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(body.as_bytes())?;
    }
    std::fs::rename(&tmp_path, &index_path)?;
    Ok(())
}

/// True if `args` contains at least one `--<flag>` token that
/// the SRD-64 vocab recognises. Used by the dispatcher to
/// distinguish "new item from CLI flags" from "select existing
/// stored item by name."
fn tail_has_vocab_flag(args: &[String]) -> bool {
    args.iter().any(|a| {
        a.starts_with("--") && nmbrs_workload::report::vocab::directive_by_cli_flag(a).is_some()
    })
}

/// SRD-64 flag-form dispatch for `nmbrs report <kind> <name>
/// [flags]`. Builds a [`ReportItem`] from the CLI flag list,
/// renders to the session's scratch directory, and (when
/// `--add` is set) routes through the workload-edit primitive.
///
/// Phase C lands the build + scratch render. Phase D wires the
/// `--add` path; until then `--add` errors with a pending
/// message so the user-facing surface is visible.
fn dispatch_new_item(
    kind: nmbrs_workload::report::Kind,
    args: &[String],
    session_dir: &std::path::Path,
    workload_path: Option<&std::path::Path>,
) {
    let mut result = match crate::report_build::build_item(kind, args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nmbrs report {}: {e}", kind.as_str());
            std::process::exit(2);
        }
    };
    // `extract_workload` peels `--workload <path>` off the
    // top-level args before dispatch, so by the time we get
    // here the builder hasn't seen it. Backfill the dispatch's
    // `workload` field from the captured path so `--add`'s
    // workload-resolution can use it.
    if result.dispatch.workload.is_none()
        && let Some(p) = workload_path
    {
        result.dispatch.workload = Some(p.to_string_lossy().into_owned());
    }

    if result.dispatch.add {
        run_add(&result, session_dir);
        return;
    }

    if result.dispatch.dry_run {
        println!("# dry-run: would render to scratch (no workload edit, --add not set)");
        println!("{}", result.item.to_yaml_directive_string());
        return;
    }

    let paths = match crate::report_scratch::scratch_paths(session_dir, &result.item) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nmbrs report {}: scratch path: {e}", kind.as_str());
            std::process::exit(2);
        }
    };

    let stub = format!(
        "<!-- {} {} -->\n\n```yaml\n{}```\n",
        kind.as_str(),
        result.item.name,
        result.item.to_yaml_directive_string(),
    );

    if result.dispatch.stdout {
        if matches!(kind, nmbrs_workload::report::Kind::Plot) && !result.dispatch.ascii {
            eprintln!(
                "--stdout is not compatible with `plot` kind; \
                 use --ascii for terminal rendering"
            );
            std::process::exit(2);
        }
        print!("{stub}");
        return;
    }

    if let Err(e) = std::fs::write(&paths.md, &stub) {
        eprintln!(
            "nmbrs report {}: write '{}': {e}",
            kind.as_str(),
            paths.md.display()
        );
        std::process::exit(2);
    }
    eprintln!("scratch render: {}", paths.md.display());
    if let Some(png) = &paths.png {
        eprintln!(
            "(png path reserved — renderer integration lands in Phase D): {}",
            png.display()
        );
    }
}

/// Phase D `--add` driver: parse the dispatch's anchor flag,
/// resolve the anchor against the active session, discover the
/// workload to mutate, and route through
/// [`nmbrs_workload::edit::add_item`].
fn run_add(result: &crate::report_build::BuildResult, session_dir: &std::path::Path) {
    use nmbrs_runtime::report_anchor::{self, AnchorFlag};

    // The builder enforced `--at` and `--contextual` are
    // mutually exclusive; here we just translate the captured
    // strings into the typed enum.
    let flag = match (&result.dispatch.at, &result.dispatch.contextual) {
        (Some(at), None) => match AnchorFlag::parse_at(at) {
            Ok(f) => f,
            Err(e) => die("nmbrs report --add", &e),
        },
        (None, Some(ctx)) => match AnchorFlag::parse_contextual(ctx) {
            Ok(f) => f,
            Err(e) => die("nmbrs report --add", &e),
        },
        (None, None) => AnchorFlag::None,
        (Some(_), Some(_)) => unreachable!("builder enforces mutual exclusion"),
    };

    let db_path = session_dir.join("metrics.db");
    let resolution = match report_anchor::resolve(&db_path, &result.item, &flag) {
        Ok(r) => r,
        Err(e) => die("nmbrs report --add", &e),
    };

    eprintln!("{}", resolution.diagnostic);

    let workload_path =
        match resolve_workload_for_add(result.dispatch.workload.as_deref(), session_dir) {
            Ok(p) => p,
            Err(e) => die("nmbrs report --add", &e),
        };

    if result.dispatch.dry_run {
        // SRD-64 §6.1 dry-run: print the chosen anchor + the
        // emit body that would land. Nothing on disk changes.
        println!("# dry-run: would write to {}", workload_path.display());
        println!("# anchor:  {}", resolution.diagnostic);
        println!("# group:   {}", result.dispatch.group);
        println!("# replace: {}", result.dispatch.replace);
        println!("---");
        println!("{}", result.item.to_yaml_directive_string());
        return;
    }

    let outcome = match nmbrs_workload::edit::add_item(
        &workload_path,
        &resolution.anchor,
        &result.dispatch.group,
        &result.item,
        result.dispatch.replace,
    ) {
        Ok(o) => o,
        Err(e) => die("nmbrs report --add", &e.to_string()),
    };

    let verb = match outcome {
        nmbrs_workload::edit::AddOutcome::Inserted => "inserted",
        nmbrs_workload::edit::AddOutcome::Replaced => "replaced",
    };
    eprintln!(
        "{verb} `{}` in {} (backup at {}.bak)",
        result.item.name,
        workload_path.display(),
        workload_path.display(),
    );
}

fn die(prefix: &str, msg: &str) -> ! {
    eprintln!("{prefix}: {msg}");
    std::process::exit(2);
}

/// Phase E `nmbrs report rename <old> <new> [flags]` driver.
///
/// SRD-64 §6.6: pure metadata edit through the Phase B
/// workload-edit primitive. Anchor stays at the existing
/// site. Collision policy:
/// - default: error if `<new>` is already in use;
/// - `--replace`: destructive overwrite of the existing
///   `<new>` item.
///
/// Flags accepted:
/// - `--workload <path>` — explicit workload override.
/// - `--replace` — destructive overwrite.
/// - `--dry-run` — print intended change, don't write.
///
/// Session-resolution flags (`--session-path`, `--session`,
/// `--session-name`) are honoured for the workload-discovery
/// fallback (via `<session>/checkpoint.jsonl::workload_path`).
fn run_rename(
    args: &[String],
    session_dir: &std::path::Path,
    extracted_workload: Option<&std::path::Path>,
) {
    let mut parsed = match parse_rename_args(args) {
        Ok(p) => p,
        Err(e) => die("nmbrs report rename", &e),
    };
    // `extract_workload` peels `--workload <path>` off before
    // dispatch reaches us; backfill so the workload-resolution
    // sees the user-supplied path.
    if parsed.workload.is_none()
        && let Some(p) = extracted_workload
    {
        parsed.workload = Some(p.to_string_lossy().into_owned());
    }

    let workload_path = match resolve_workload_for_add(parsed.workload.as_deref(), session_dir) {
        Ok(p) => p,
        Err(e) => die("nmbrs report rename", &e),
    };

    if parsed.dry_run {
        println!(
            "# dry-run: would rename '{}' → '{}' in {}",
            parsed.old,
            parsed.new,
            workload_path.display()
        );
        if parsed.replace {
            println!("# replace: drop existing '{}' if present", parsed.new);
        }
        return;
    }

    if let Err(e) =
        nmbrs_workload::edit::rename_item(&workload_path, &parsed.old, &parsed.new, parsed.replace)
    {
        die("nmbrs report rename", &e.to_string());
    }
    eprintln!(
        "renamed `{}` → `{}` in {} (backup at {}.bak)",
        parsed.old,
        parsed.new,
        workload_path.display(),
        workload_path.display(),
    );
}

#[derive(Debug)]
struct RenameArgs {
    old: String,
    new: String,
    workload: Option<String>,
    replace: bool,
    dry_run: bool,
}

fn parse_rename_args(args: &[String]) -> Result<RenameArgs, String> {
    let mut old: Option<String> = None;
    let mut new: Option<String> = None;
    let mut workload: Option<String> = None;
    let mut replace = false;
    let mut dry_run = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--replace" => {
                replace = true;
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--workload" => {
                workload = Some(
                    args.get(i + 1)
                        .ok_or("--workload requires a value")?
                        .clone(),
                );
                i += 2;
            }
            // Session-resolution flags pass through (consumed
            // by the top-level resolver before run_rename is
            // called).
            "--session" | "--session-path" | "--session-name" | "--db" => {
                i += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag '{other}'"));
            }
            _ => {
                if old.is_none() {
                    old = Some(arg.clone());
                } else if new.is_none() {
                    new = Some(arg.clone());
                } else {
                    return Err(format!(
                        "unexpected positional '{arg}'; \
                         usage: nmbrs report rename <old> <new> [flags]"
                    ));
                }
                i += 1;
            }
        }
    }

    Ok(RenameArgs {
        old: old.ok_or("missing <old> name; usage: nmbrs report rename <old> <new>")?,
        new: new.ok_or("missing <new> name; usage: nmbrs report rename <old> <new>")?,
        workload,
        replace,
        dry_run,
    })
}

/// Resolve which workload YAML the `--add` should mutate.
///
/// Order of precedence:
/// 1. `--workload <path>` if explicitly passed.
/// 2. `<session>/checkpoint.jsonl::workload_path`, when the
///    session was launched with a workload-file invocation
///    that recorded the path.
/// 3. Error with a remediation hint.
fn resolve_workload_for_add(
    explicit: Option<&str>,
    session_dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    if let Some(p) = explicit {
        let path = std::path::PathBuf::from(p);
        if !path.exists() {
            return Err(format!("--workload '{}' does not exist", path.display(),));
        }
        return Ok(path);
    }
    // Pre-SRD-44a, this fallback tried to read a
    // `workload_path` field from `checkpoint.json`, but the
    // checkpoint schema (then or now) never carries that
    // field, so the fallback path could only ever return an
    // error. SRD-44a converted the file to a JSONL event log,
    // which makes a one-shot `serde_json::from_slice` parse
    // wrong anyway. Surface the missing-flag diagnostic
    // directly until a future event type carries the workload
    // path explicitly.
    Err(format!(
        "no --workload <path> given; pass --workload <file.yaml> to \
         point at the workload to mutate (session at {})",
        session_dir.display(),
    ))
}

/// Pass-through for the flag-form invocation: the user typed
/// `nmbrs plot --name X` (or `nmbrs table --filter k=v`). Forward
/// everything to the renderer, including the workload= token
/// if present.
fn forward_renderer_flags(
    kind_filter: KindFilter,
    args: &[String],
    workload_arg: Option<&str>,
    session_db: Option<&Path>,
) {
    let mut full: Vec<String> = Vec::new();
    if let Some(w) = workload_arg {
        full.push(w.to_string());
    }
    // Re-inject the resolved session db as an explicit `--db`
    // (overridable by anything in `args` that supplies its own
    // `--db`) so the downstream renderer doesn't fall back to
    // `logs/latest/metrics.db` after we stripped `--session`
    // out of `args` in `extract_workload`.
    if let Some(db) = session_db {
        let already_has_db = args.iter().any(|a| a == "--db" || a.starts_with("--db="));
        if !already_has_db {
            full.push("--db".to_string());
            full.push(db.to_string_lossy().into_owned());
        }
    }
    full.extend(args.iter().cloned());
    match kind_filter {
        KindFilter::Plot => crate::plot_metrics::plot_metrics_command(&full),
        KindFilter::Table => crate::summary::summary_command(&full),
        KindFilter::Any => {
            // Normally pre-empted by the dispatch-time check in
            // `report_command`; kept for callers that reach the
            // kindless arm some other way. Same contract: an unknown
            // flag is named as the mistake, not wrapped in dispatch
            // guidance about a form the user wasn't using.
            if let Some(first) = args.iter().find(|a| a.starts_with("--")) {
                reject_if_unknown_flag(first);
            }
            eprintln!(
                "nmbrs report: flag-form selection requires a kind \
                (use `nmbrs plot --<flag>...` or `nmbrs table --<flag>...`)"
            );
            std::process::exit(2);
        }
    }
}

/// Exit(2) with an "unknown option" error when `token` (a `--flag` or
/// `--flag=value` spelling) is in NO vocabulary the CLI accepts —
/// checked against the derived closed surface
/// ([`crate::completion::known_flags`]: the spec walk, the SRD-64
/// report vocab, and the renderer flag lists), with prefix
/// suggestions drawn from that same set. No-op for known flags.
fn reject_if_unknown_flag(token: &str) {
    let flag = token.split('=').next().unwrap_or(token);
    if crate::completion::known_flags().contains(flag) {
        return;
    }
    let mut hits: Vec<String> = crate::completion::known_flags()
        .iter()
        .filter(|c| c.starts_with(flag))
        .cloned()
        .collect();
    hits.sort();
    eprintln!(
        "nmbrs report: unknown option '{flag}'.{}",
        nmbrs_workload::suggest::did_you_mean(&hits)
    );
    std::process::exit(2);
}

#[derive(Debug, Clone, Copy)]
pub enum KindFilter {
    Any,
    Plot,
    Table,
}

impl KindFilter {
    fn matches(&self, k: nmbrs_workload::report::Kind) -> bool {
        use nmbrs_workload::report::Kind;
        matches!(
            (self, k),
            (KindFilter::Any, _)
                | (KindFilter::Plot, Kind::Plot)
                | (KindFilter::Table, Kind::Table)
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedItem {
    pub name: String,
    pub kind: nmbrs_workload::report::Kind,
    pub label: Option<String>,
    pub body: String,
    /// Resolved palette name/index after the cascade (workload
    /// `defaults` → group `defaults` → item style). `None` ⇒ no
    /// override; renderer uses the default palette.
    pub palette: Option<String>,
    /// SRD-46 line dash style (`solid`, `dashed`, `dotted`,
    /// `none`).
    pub line: Option<String>,
    /// Stroke width in pixels.
    pub width: Option<f32>,
    /// Marker shape (`none`, `circle`, `square`, `triangle`,
    /// `diamond`, `plus`, `cross`).
    pub marker: Option<String>,
    /// Marker size (radius in pixels).
    pub marker_size: Option<f32>,
    /// SRD-46 target output file. `None` ⇒ default
    /// `summary.md`. Set by a preceding `file <filename>`
    /// directive in the same group.
    pub target_file: Option<String>,
    /// Per-series style overrides — one entry per `series
    /// <key>=<value>:<directives>` body line. Each entry binds a
    /// (key, value) discriminator to a `Style` with the same
    /// fields the item-level cascade uses (line / width /
    /// marker / size / color / palette). Forwarded to the plot
    /// renderer via `--series-override key=value:k=v k=v` so
    /// the per-series loop in `draw_chart` can substitute the
    /// override for the matching series's palette default.
    pub series_overrides: Vec<nmbrs_workload::report::SeriesOverride>,
    /// SRD-46 plot-only `with-table: true` directive — when
    /// set on a `Kind::Plot` item, the renderer emits a
    /// companion table immediately after the plot in the
    /// same markdown file. The table reuses the plot's
    /// query data (each series query becomes one column).
    pub with_table: bool,
    /// `with-tables: [label1, label2, …]` faceting list —
    /// when non-empty, the renderer fans out one companion
    /// table per distinct value tuple of the listed labels
    /// (in addition to / replacing the singular form).
    pub with_tables: Vec<String>,
    /// SRD-46 output routing after the cascade (workload
    /// `defaults` → group `defaults` → item `to`). `None` ⇒
    /// nothing declared it; the render entry point supplies
    /// the default, which differs between automatic
    /// end-of-run rendering and an explicit `nmbrs report`.
    pub destinations: Option<Vec<nmbrs_workload::report::Destination>>,
}

/// The first db named by a `--db` flag, or `None`.
///
/// Accepts `--db <path>` and `--db=<path>`, and takes the first entry of a
/// comma-separated list — the same spellings the downstream renderer's parser
/// accepts, and the same "first is primary" rule it uses to anchor output.
fn db_flag_path(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        let list = if a == "--db" {
            iter.next().map(String::as_str)
        } else {
            a.strip_prefix("--db=")
        };
        if let Some(list) = list {
            if let Some(first) = list.split(',').map(str::trim).find(|s| !s.is_empty()) {
                return Some(PathBuf::from(first));
            }
        }
    }
    None
}

/// The workload yaml a session's runs recorded. `workload_file`
/// (a path, persisted at end-of-run) wins; a LIVE session only has
/// the start-of-run `workload` row — the arg as passed, often a
/// bare name — which goes through the same name→path search `nmbrs
/// run` uses (cwd, `workloads/`, `adapters/*/workloads/`,
/// `nmbrs/examples/workloads/`). `None` when the db is absent/unreadable
/// or no run recorded either row; an unresolvable name is returned
/// as-is so the caller's error can say what the session recorded.
fn workload_recorded_in(db_path: &Path) -> Option<PathBuf> {
    if !db_path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open(db_path).ok()?;
    let meta =
        |key: &str| nmbrs_metrics::reporters::sqlite::latest_execution_metadata_value(&conn, key);
    if let Some(path) = meta("workload_file") {
        return Some(PathBuf::from(path));
    }
    let name = meta("workload")?;
    Some(PathBuf::from(
        crate::cli::resolve_workload_path(&name).unwrap_or(name),
    ))
}

fn extract_workload(args: &[String]) -> (Option<PathBuf>, Vec<String>) {
    // Global flags consumed elsewhere (`--session*` by
    // `read_session_dir`, `workload=` here, lifecycle flags by
    // `purge_stale_sessions_at_startup`). Peel them so the
    // dispatch loop's `rest.first()` classification sees only
    // the report-subcommand vocabulary (`all`, `figure`,
    // glob, `--name`, etc.). Without this, `nmbrs report
    // --session local/foo` would route to flag-form because
    // `--session` is `--`-prefixed.
    const FLAGS_WITH_VALUES: &[&str] = &[
        "--session",
        "--session-name",
        "--session-path",
        "--session-reuse",
        "--session-keep",
        "--session-shelflife",
        "--resume",
        "--polydat-lib",
    ];
    const BOOL_FLAGS: &[&str] = &[
        "--strict",
        "--no-prompt",
        "--resume-latest",
        "--force-retry-failed",
        // `--rebuild` wipes the report markdown files that
        // the workload's `report:` block declares before
        // rendering, so a workload that *removed* a plot
        // since the last `nmbrs report` doesn't leave the
        // stale section sitting in summary.md. Consumed by
        // `is_rebuild_mode`; stripped here so it doesn't
        // confuse the dispatch loop.
        "--rebuild",
        // `--clean` (only honored alongside `all`) wipes
        // every `.png` and `.md` file in the session output
        // directory before rendering. Stronger than
        // `--rebuild` — sweeps individual artifact files
        // too. Consumed by `is_clean_mode`.
        "--clean",
    ];
    let mut workload_path: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        // Capture the workload path from any of:
        //   workload=<path>        (positional key=value form)
        //   --workload <path>      (space-separated flag form)
        //   --workload=<path>      (= form)
        if let Some(p) = a.strip_prefix("workload=") {
            workload_path = Some(PathBuf::from(p));
            i += 1;
            continue;
        }
        if let Some(p) = a.strip_prefix("--workload=") {
            workload_path = Some(PathBuf::from(p));
            i += 1;
            continue;
        }
        if a == "--workload" {
            if let Some(v) = args.get(i + 1) {
                workload_path = Some(PathBuf::from(v));
                i += 2;
                continue;
            }
            // Trailing `--workload` with no value — let the
            // downstream parser surface the error.
            i += 1;
            continue;
        }
        if FLAGS_WITH_VALUES.contains(&a.as_str()) {
            // Skip the flag and its value.
            i += 2;
            continue;
        }
        if FLAGS_WITH_VALUES
            .iter()
            .any(|f| a.starts_with(&format!("{f}=")))
        {
            i += 1;
            continue;
        }
        if BOOL_FLAGS.contains(&a.as_str()) {
            i += 1;
            continue;
        }
        rest.push(a.clone());
        i += 1;
    }
    (workload_path, rest)
}

/// Map a parsed [`nmbrs_workload::report::ReportItem`] (plus its
/// effective style and a param map for `{name}` substitution)
/// into a [`ResolvedItem`]. Single conversion site shared by
/// both the workload-source path and the session-db fallback —
/// any new field on `ReportItem` needs to be threaded through
/// here exactly once.
fn resolve_item(
    item: &nmbrs_workload::report::ReportItem,
    style: &nmbrs_workload::report::Style,
    params: &std::collections::HashMap<String, String>,
) -> ResolvedItem {
    let expand = |s: &str| nmbrs_runtime::runner::expand_workload_params(s, params);
    ResolvedItem {
        name: item.name.clone(),
        kind: item.kind,
        label: item.label.as_deref().map(expand),
        body: expand(&item.body),
        palette: style.palette.clone(),
        line: style.line.clone(),
        width: style.width,
        marker: style.marker.clone(),
        marker_size: style.size,
        target_file: item.target_file.as_deref().map(expand),
        series_overrides: style.series.clone(),
        with_table: item.with_table,
        with_tables: item.with_tables.clone(),
        destinations: style.destinations.clone(),
    }
}

/// Filter the resolved items to `Kind::Plot` and project to
/// `(name, body)` pairs — the shape `plot_metrics` consumes
/// when looking up a named plot by `--name` or by `all`.
/// Single source of truth: both the report-rendering pipeline
/// and the plot-rendering pipeline route through
/// [`resolve_items`], so `with-table` / `target` / style
/// directive stripping happens in exactly one place.
pub(crate) fn plot_body_specs(
    workload_path: Option<&Path>,
    session_db: Option<&Path>,
) -> Result<Vec<(String, String)>, String> {
    use nmbrs_workload::report::Kind;
    let (items, _) = resolve_items(workload_path, session_db, false)?;
    Ok(items
        .into_iter()
        .filter(|i| matches!(i.kind, Kind::Plot))
        .map(|i| {
            // The report parser tokenizes `style key=value:...`
            // lines OUT of the body into `series_overrides` —
            // the report-cmd path then converts them into
            // `--style` CLI args at dispatch. But the
            // `nmbrs plot --name X --workload Y` path goes
            // directly to `parse_spec(body)` and would miss
            // them. Re-append in the canonical
            // `style key=value:k=v k=v` shape so the
            // plot-body parser (which now recognises this
            // form) picks them up.
            let mut body = i.body;
            for so in &i.series_overrides {
                if !body.ends_with('\n') && !body.is_empty() {
                    body.push('\n');
                }
                body.push_str("style ");
                body.push_str(&so.key);
                body.push('=');
                body.push_str(&so.value);
                body.push(':');
                let mut first = true;
                for line in so.style.scalar_directive_lines() {
                    if !first {
                        body.push(' ');
                    }
                    first = false;
                    body.push_str(&line);
                }
                body.push('\n');
            }
            (i.name, body)
        })
        .collect())
}

/// Extract every report item from a parsed workload, with the workload's params
/// interpolated.
///
/// Shared by both resolution paths so an item resolved from a stored
/// `workload_yaml` is identical to one resolved from the file on disk — the two
/// must not drift, or the same report would render differently depending on how
/// it was reached.
fn items_from_workload(
    workload: &nmbrs_workload::model::Workload,
    synthesized_only: bool,
    scenario: Option<&str>,
) -> Result<(Vec<ResolvedItem>, bool), String> {
    // Report items routinely contain `{cql_dialect}`-style placeholders that
    // operators expect rendered with the workload's declared values. Expand once
    // here so every downstream consumer (markdown assembler, plot renderer) sees
    // resolved literals.
    let params: std::collections::HashMap<String, String> = workload.params.clone();
    // SRD-109 — no explicit `report:` block ⇒ SYNTHESIZE one from the
    // workload's structural fixtures (key_metrics designations +
    // anchors) and feed it through the SAME parse_report entry the
    // YAML path uses. Well-formedness violations (unknown family,
    // silent flatten through a non-anchored sweep) are report-time
    // errors by contract — never warnings.
    let synthesized;
    let was_synthesized = synthesized_only || workload.report.groups.is_empty();
    let report: &nmbrs_workload::report::Report =
        if synthesized_only || workload.report.groups.is_empty() {
            let mapping = nmbrs_workload::report_synth::synthesize_forced_for(workload, scenario)?;
            let parsed = nmbrs_workload::report::parse_report(&mapping).map_err(|e| {
                format!(
                    "synthesized report failed its own \
                                      grammar (bug in synthesis): {e}"
                )
            })?;
            synthesized = parsed.report;
            &synthesized
        } else {
            &workload.report
        };
    let mut out: Vec<ResolvedItem> = Vec::new();
    for group in &report.groups {
        for item in &group.items {
            let style = report.effective_style(group, item);
            out.push(resolve_item(item, &style, &params));
        }
    }
    Ok((out, was_synthesized))
}

pub(crate) fn resolve_items(
    workload_path: Option<&Path>,
    session_db: Option<&Path>,
    synthesized_only: bool,
) -> Result<(Vec<ResolvedItem>, bool), String> {
    // The executed scenario, when a session db is reachable: scopes
    // synthesis to what actually ran, so views don't carry
    // structurally-empty columns from sibling scenarios.
    let scenario: Option<String> = {
        let db = session_db
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db);
        db.exists()
            .then(|| rusqlite::Connection::open(&db).ok())
            .flatten()
            .and_then(|c| {
                nmbrs_metrics::reporters::sqlite::latest_execution_metadata_value(&c, "scenario")
            })
    };
    let scenario = scenario.as_deref();
    if let Some(p) = workload_path {
        let resolved = crate::cli::resolve_workload_path(&p.to_string_lossy())
            .map(PathBuf::from)
            .unwrap_or_else(|| p.to_path_buf());
        if !resolved.exists() {
            return Err(format!("workload '{}' not found", resolved.display()));
        }
        let workload = nmbrs_workload::parse::parse_workload_from_path(
            &resolved,
            &std::collections::HashMap::new(),
        )?;
        // Workload-param interpolation: report items (the
        // `label "..."` and the body lines) routinely
        // contain `{cql_dialect}`-style placeholders that
        // operators expect to render with the workload's
        // declared param values. Expand them once here so
        // every downstream consumer (the markdown
        // assembler, the plot renderer that parses the
        // body) sees the resolved literals.
        items_from_workload(&workload, synthesized_only, scenario)
    } else {
        // Db fallback: read `report.<name>` rows from the
        // session db's session_metadata table (SRD-46). Each
        // value carries the kind keyword + name + optional
        // `label "..."` + spec body — the same shape the
        // report parser ingests.
        let db_path = session_db
            .map(PathBuf::from)
            .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db);
        if !db_path.exists() {
            return Ok((Vec::new(), false));
        }
        let conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return Ok((Vec::new(), false)),
        };
        // Pull the workload's persisted params first so we
        // can expand `{name}` placeholders in stored item
        // labels / bodies / target_file paths. The runner
        // writes one `param.<key> → <value>` row per
        // declared workload param at session start
        // (`runner.rs:774`); here we read them back for the
        // expansion. Same substitution as the YAML path.
        // Latest execution's params + report defs (per-execution
        // metadata), with legacy session_metadata fallback.
        let mut params: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (k, v) in
            nmbrs_metrics::reporters::sqlite::latest_execution_metadata_like(&conn, "param.%")
        {
            if let Some(name) = k.strip_prefix("param.") {
                params.insert(name.to_string(), v);
            }
        }
        let rows =
            nmbrs_metrics::reporters::sqlite::latest_execution_metadata_like(&conn, "report.%");
        let mut out: Vec<(Option<usize>, ResolvedItem)> = Vec::new();
        let default_style = nmbrs_workload::report::Style::default();
        for row in rows {
            // The db value is one item's persisted form (header
            // line + indented directives, per
            // `ReportItem::to_yaml_directive_string`). Delegate
            // to the workload-side parser so directive handling
            // lives in one place and stays in lockstep with the
            // YAML path.
            if row.0.strip_prefix("report.").is_none() {
                continue;
            }
            match nmbrs_workload::report::parse_persisted_item(&row.1) {
                Ok(item) => out.push((item.order, resolve_item(&item, &default_style, &params))),
                Err(_) => continue,
            }
        }
        if !out.is_empty() {
            // Restore declaration order from the persisted
            // `order <n>` directive; legacy rows without it keep
            // their key order, after every ordered item (stable
            // sort).
            out.sort_by_key(|(order, _)| order.unwrap_or(usize::MAX));
            return Ok((out.into_iter().map(|(_, item)| item).collect(), false));
        }
        // LIVE-SESSION fallback: no `report.*` rows yet. Those are persisted when
        // a run ENDS, so during a run the db has none — which used to force
        // `workload=<file>` and made the same report reachable one way live and
        // another way afterwards.
        //
        // The run start already stored the entire workload source under
        // `workload_yaml`, so nothing new is needed: parse the report block out of
        // that. The command therefore behaves identically at any point in a run's
        // life, and resolves against the workload as it was WHEN THE RUN STARTED
        // rather than whatever the file says now — which is the more truthful
        // source for a report about that run.
        let yaml = nmbrs_metrics::reporters::sqlite::latest_execution_metadata_like(
            &conn,
            "workload_yaml",
        )
        .into_iter()
        .find(|(k, _)| k == "workload_yaml")
        .map(|(_, v)| v);
        if let Some(yaml) = yaml {
            match nmbrs_workload::parse::parse_workload(&yaml, &std::collections::HashMap::new()) {
                Ok(w) => return items_from_workload(&w, false, scenario),
                Err(e) => {
                    eprintln!(
                        "nmbrs report: stored workload_yaml did not parse \
                               ({e}); pass `workload=<file>` to override"
                    );
                }
            }
        }
        Ok((out.into_iter().map(|(_, item)| item).collect(), false))
    }
}

/// One-line content hint for a figure whose author gave no
/// label: tables show their grouping and column names, plots
/// their axis metrics and series keys — the listing must say
/// what each item REPORTS, not just what it is called.
fn content_hint(item: &ResolvedItem) -> String {
    use nmbrs_workload::report::Kind;
    match item.kind {
        Kind::Table => {
            let cfg = nmbrs_workload::model::SummaryConfig::parse(&item.body);
            let cols: Vec<&str> = if cfg.metricsql_columns.is_empty() {
                cfg.columns.iter().map(String::as_str).collect()
            } else {
                cfg.metricsql_columns
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect()
            };
            let cols = if cols.is_empty() {
                "(all gauges)".to_string()
            } else {
                cols.join(", ")
            };
            if cfg.group_by.is_empty() {
                format!("columns: {cols}")
            } else {
                format!("by {}: {cols}", cfg.group_by.join(","))
            }
        }
        Kind::Plot => {
            let mut axes: Vec<String> = Vec::new();
            let mut series = String::new();
            for line in item.body.lines() {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix("series") {
                    series = rest.trim_start_matches([':', ' ']).to_string();
                }
                for prefix in ["y1:", "y2:", "y3:", "y4:", "y:", "x1:", "x:"] {
                    if let Some(expr) = l.strip_prefix(prefix) {
                        // Pull up to two metric identifiers (paired
                        // mode carries x and y in one tuple).
                        let mut rest = expr;
                        for _ in 0..2 {
                            match crate::plot_metrics::metric_name_from_query(rest) {
                                Some(m) => {
                                    let idx =
                                        rest.find(&m).map(|i| i + m.len()).unwrap_or(rest.len());
                                    axes.push(m);
                                    rest = &rest[idx..];
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
            axes.dedup();
            let mut hint = axes.join(" vs ");
            if hint.is_empty() {
                hint = "(no axis queries)".to_string();
            }
            if !series.is_empty() {
                hint.push_str(&format!("; series {series}"));
            }
            hint
        }
        _ => String::new(),
    }
}

/// Flat (name → mtime) snapshot of the files directly under the
/// report output root. Input to [`artifacts_written_since`].
fn artifact_snapshot(
    root: &Path,
) -> std::collections::HashMap<std::path::PathBuf, std::time::SystemTime> {
    let mut out = std::collections::HashMap::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file()
                && let Ok(md) = e.metadata()
                && let Ok(mtime) = md.modified()
            {
                out.insert(p, mtime);
            }
        }
    }
    out
}

/// File names (relative to the root) created or modified since the
/// snapshot, sorted. `index.md` is excluded — the per-invocation
/// index rewrite would name it every time.
fn artifacts_written_since(
    root: &Path,
    before: &std::collections::HashMap<std::path::PathBuf, std::time::SystemTime>,
) -> Vec<String> {
    let mut out: Vec<String> = artifact_snapshot(root)
        .into_iter()
        .filter(|(p, mtime)| before.get(p) != Some(mtime))
        .filter_map(|(p, _)| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| n != "index.md")
        .collect();
    out.sort();
    out
}

/// Wrap a listing hint at its comma boundaries so long column lists
/// never wrap mid-name in the terminal; continuation lines align
/// under the hint column. Width comes from the tty (single line when
/// stderr isn't one — piped output stays grep-friendly).
fn wrap_hint(text: &str, indent: usize) -> String {
    match nmbrs_runtime::activity::terminal_cols() {
        Some(width) => wrap_hint_to(text, indent, width),
        None => text.to_string(),
    }
}

fn wrap_hint_to(text: &str, indent: usize, width: usize) -> String {
    let avail = width.saturating_sub(indent).max(20);
    if text.chars().count() <= avail {
        return text.to_string();
    }
    let mut out = String::new();
    let mut line_len = 0usize;
    for (i, part) in text.split(", ").enumerate() {
        let plen = part.chars().count();
        if i > 0 {
            if line_len + 2 + plen > avail {
                out.push_str(",\n");
                out.push_str(&" ".repeat(indent));
                line_len = 0;
            } else {
                out.push_str(", ");
                line_len += 2;
            }
        }
        out.push_str(part);
        line_len += plen;
    }
    out
}

fn print_listing(items: &[ResolvedItem], filter: KindFilter) {
    use nmbrs_workload::report::Kind;
    let kind_label = match filter {
        KindFilter::Any => "items",
        KindFilter::Plot => "plots",
        KindFilter::Table => "tables",
    };
    let total = items.iter().filter(|i| filter.matches(i.kind)).count();
    if total == 0 {
        eprintln!("(no report items defined)");
        return;
    }
    println!("# Report {kind_label} ({total} total)");

    // Figure numbers count only plot+table; text/file are
    // skipped (SRD-46). The number prints alongside each
    // figure; text shows a `T` prefix; file shows the section
    // header.
    let mut fig_num: usize = 0;
    let mut last_target: Option<String> = None;
    for item in items.iter() {
        if !filter.matches(item.kind) {
            // Bump fig counter to keep numbering stable even
            // when the listing is filtered.
            if item.kind.is_figure() {
                fig_num += 1;
            }
            continue;
        }
        // Section banner when target_file changes.
        let this_target = item.target_file.clone();
        if this_target != last_target {
            match this_target.as_deref() {
                Some(t) => println!("\nfile {t}:"),
                None => println!("\n(default → summary.md):"),
            }
            last_target = this_target;
        }
        match item.kind {
            Kind::Plot | Kind::Table => {
                fig_num += 1;
                let hint = content_hint(item);
                let display = match item.label.as_deref() {
                    Some(l) if !l.is_empty() => format!("\"{l}\" — {hint}"),
                    _ => hint,
                };
                let prefix = format!(
                    "  {fig_num:3} — {name:24} {kind:6} ",
                    name = item.name,
                    kind = item.kind.as_str()
                );
                let indent = prefix.chars().count();
                println!("{prefix}{}", wrap_hint(&display, indent));
            }
            Kind::Text => {
                let label = item.label.as_deref().unwrap_or("");
                let preview = item.body.lines().next().unwrap_or("").trim();
                let preview = if preview.len() > 40 {
                    format!("{}…", &preview[..40])
                } else {
                    preview.to_string()
                };
                let display = if !label.is_empty() {
                    label.to_string()
                } else {
                    preview
                };
                println!(
                    "    T — {name:24} {kind:6} \"{display}\"",
                    name = item.name,
                    kind = item.kind.as_str()
                );
            }
            Kind::File => {
                let label = item.label.as_deref().unwrap_or("");
                println!(
                    "    F — {name:24} {kind:6} \"{label}\"",
                    name = item.name,
                    kind = item.kind.as_str()
                );
            }
            Kind::Details => {
                let label = item.label.as_deref().unwrap_or("run details");
                println!(
                    "    D — {name:24} {kind:6} \"{label}\"",
                    name = item.name,
                    kind = item.kind.as_str()
                );
            }
        }
    }

    // Closing summary: what this report will actually produce —
    // figure counts by kind, total table columns, and text
    // sections — so the listing reads as a report inventory, not
    // just a name index.
    let shown: Vec<&ResolvedItem> = items.iter().filter(|i| filter.matches(i.kind)).collect();
    let tables = shown
        .iter()
        .filter(|i| matches!(i.kind, Kind::Table))
        .count();
    let plots = shown
        .iter()
        .filter(|i| matches!(i.kind, Kind::Plot))
        .count();
    let texts = shown
        .iter()
        .filter(|i| matches!(i.kind, Kind::Text))
        .count();
    let files = shown
        .iter()
        .filter(|i| matches!(i.kind, Kind::File))
        .count();
    let columns: usize = shown
        .iter()
        .filter(|i| matches!(i.kind, Kind::Table))
        .map(|i| {
            let cfg = nmbrs_workload::model::SummaryConfig::parse(&i.body);
            if cfg.metricsql_columns.is_empty() {
                cfg.columns.len()
            } else {
                cfg.metricsql_columns.len()
            }
        })
        .sum();
    let mut parts: Vec<String> = Vec::new();
    if tables > 0 {
        parts.push(format!("{tables} table(s) totalling {columns} column(s)"));
    }
    if plots > 0 {
        parts.push(format!("{plots} plot(s)"));
    }
    if texts > 0 {
        parts.push(format!("{texts} text section(s)"));
    }
    if files > 0 {
        parts.push(format!("{files} named report file(s)"));
    }
    if !parts.is_empty() {
        println!("\n{} figure(s): {}", tables + plots, parts.join(", "));
    }

    // Name the files rendering will produce: every item's target
    // markdown (default summary.md) plus each table's own
    // `<name>_table.md`. Plot image names depend on renderer flags,
    // so they are counted, not guessed.
    let mut outputs: Vec<String> = Vec::new();
    for i in &shown {
        let target = i
            .target_file
            .clone()
            .unwrap_or_else(|| "summary.md".to_string());
        if !outputs.contains(&target) {
            outputs.push(target);
        }
        if matches!(i.kind, Kind::Table) {
            let t = format!("{}_table.md", i.name);
            if !outputs.contains(&t) {
                outputs.push(t);
            }
        }
    }
    if !outputs.is_empty() {
        outputs.sort();
        let listing = outputs.join(", ");
        let prefix = "renders to: ";
        let mut tail = String::new();
        if plots > 0 {
            tail = format!(" (+{plots} plot image(s), named per figure)");
        }
        println!(
            "{prefix}{}{tail}",
            wrap_hint(&listing, prefix.chars().count())
        );
    }
}

fn render_all(
    items: &[ResolvedItem],
    filter: KindFilter,
    passthrough: &[String],
    workload_arg: Option<&str>,
    output_root: &Path,
    session_db: Option<&Path>,
    strict: bool,
) -> Vec<String> {
    // SRD-46: figure numbers count only plot+table items, in
    // their order across the whole resolved item list. The
    // counter advances even when the kind filter excludes the
    // item, so numbers stay stable regardless of which subset
    // the operator renders.
    let mut fig_num: usize = 0;
    let mut failures: Vec<String> = Vec::new();
    let to_render: usize = items.iter().filter(|i| filter.matches(i.kind)).count();
    if to_render == 0 {
        // Items resolved but the kind filter excluded all of
        // them — distinguishable from the "no items at all"
        // case the caller already warned about.
        eprintln!(
            "nmbrs report: 0 of {} item(s) match the kind filter \
             (rendering nothing)",
            items.len(),
        );
        return failures;
    }
    eprintln!("nmbrs report: rendering {} item(s)…", to_render);
    let mut idx = 0;
    for item in items.iter() {
        if item.kind.is_figure() {
            fig_num += 1;
        }
        if !filter.matches(item.kind) {
            continue;
        }
        idx += 1;
        // Per-item heading so the operator can map the
        // downstream renderer output (plot points, table
        // rows, error lines) back to the item it came from.
        // The figure-number prefix matches `nmbrs report list`
        // / `nmbrs report figure N` so cross-referencing
        // works.
        let fig_label = if item.kind.is_figure() {
            format!("[fig {}] ", fig_num)
        } else {
            String::new()
        };
        eprintln!(
            "  ({}/{}) {}{} {}",
            idx,
            to_render,
            fig_label,
            item.kind.as_str(),
            item.name
        );
        if let Err(e) = render_one(
            fig_num,
            item,
            passthrough,
            workload_arg,
            output_root,
            session_db,
        ) {
            classify_render_error(e, strict, &mut failures);
        }
    }
    eprintln!(
        "nmbrs report: rendered {} item(s); {} failure(s)",
        to_render,
        failures.len()
    );
    failures
}

/// Detect SRD-15 strict mode from the args passed to
/// `report_command`. Mirrors the convention used by the
/// runner: `--strict` literally on the arg list, or the
/// `NMBRS_STRICT` env var set. When strict is on, no-data
/// figure-render errors keep their hard-failure semantics.
fn is_strict_mode(args: &[String]) -> bool {
    args.iter().any(|a| a == "--strict") || std::env::var("NMBRS_STRICT").is_ok()
}

/// True when `--rebuild` is on the arg list. Activates the
/// "fresh markdown" code path that deletes every report file
/// the workload would write to *before* the renderer runs.
/// Use case: the operator removed a plot from the workload's
/// `report:` block and wants the resulting markdown to match
/// the new declaration set without orphan sections from the
/// previous render.
fn is_rebuild_mode(args: &[String]) -> bool {
    args.iter().any(|a| a == "--rebuild") || std::env::var("NMBRS_REPORT_REBUILD").is_ok()
}

/// True when `--clean` is on the arg list. Activates the
/// blanket-wipe code path that removes every `.png` and
/// `.md` file from the session output directory before
/// rendering. Use case: the operator has reshaped the
/// workload's `report:` block (renamed / removed items)
/// and wants the post-render directory to be exactly the
/// current declaration set — no orphan artifacts from
/// prior runs.
fn is_clean_mode(args: &[String]) -> bool {
    args.iter().any(|a| a == "--clean") || std::env::var("NMBRS_REPORT_CLEAN").is_ok()
}

/// Remove every top-level `.png` and `.md` file under
/// `output_root`. Called before `report all` when
/// `--clean` is set. Non-recursive on purpose — we only
/// touch artifacts in the session's own directory, not
/// any nested `metrics/` / `traces/` / `vectordata/`
/// directories that other systems own.
///
/// Best-effort: missing-files and read-dir failures are
/// reported but don't abort. The render itself will fail
/// with a clearer message if it can't write.
fn clean_wipe_artifacts(output_root: &Path) {
    let entries = match std::fs::read_dir(output_root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "nmbrs report: --clean: could not read directory '{}': {e}",
                output_root.display(),
            );
            return;
        }
    };
    let mut removed: usize = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "png" && ext != "md" {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
            }
            Err(e) => eprintln!(
                "nmbrs report: --clean: could not remove '{}': {e}",
                path.display(),
            ),
        }
    }
    eprintln!(
        "nmbrs report: --clean removed {removed} artifact file(s) from '{}'",
        output_root.display(),
    );
}

/// Delete every report markdown file that the resolved
/// items would write to. Called before rendering when
/// `--rebuild` is set so a re-render reflects the *current*
/// workload declaration set, not a union of every prior
/// run's sections.
///
/// The wipe is scoped to declared-target files only: items
/// without a `target_file` set fall through to the default
/// `summary.md`, and that's deleted exactly once even when
/// many items share it. Files outside the resolved-target
/// set are left untouched — ad-hoc `nmbrs report scratch`
/// output, hand-edited notes, prior-run images, all
/// preserved.
fn rebuild_wipe_targets(items: &[ResolvedItem], output_root: &Path) {
    use std::collections::HashSet;
    let mut targets: HashSet<PathBuf> = HashSet::new();
    for item in items {
        let target = item.target_file.as_deref().unwrap_or("summary.md");
        targets.insert(output_root.join(target));
    }
    for path in &targets {
        match std::fs::remove_file(path) {
            Ok(()) => eprintln!("nmbrs report: --rebuild removed '{}'", path.display()),
            // ENOENT is expected — first-run rebuild has
            // nothing to wipe; that's fine. Other errors
            // (permission, I/O) print so the operator
            // notices, but don't abort the render — the
            // renderer will fail with a clearer message
            // if it can't write.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!(
                "nmbrs report: --rebuild: could not remove '{}': {e}",
                path.display(),
            ),
        }
    }
}

/// Classify a render error as a warning vs failure based on
/// strict-mode and the `[no-data]` sentinel. In strict mode
/// every error is a failure (legacy behaviour); otherwise
/// no-data errors print as warnings and don't trigger a
/// nonzero exit. Used by every render-batch entry point.
/// Post-run entry (SRD-46 auto-render): render every report item
/// the runner persisted into the session db — plots, tables, text
/// sections, and `file` targets — through the SAME pipeline as
/// `nmbrs report all`, but non-exiting: the run's outcome is
/// already decided, so no-data conditions downgrade to warnings
/// (unless NMBRS_STRICT) and real render failures come back to the
/// caller instead of `process::exit`-ing.
pub fn render_session_reports(db_path: &Path) -> Result<usize, Vec<String>> {
    let (mut items, _) = resolve_items(None, Some(db_path), false).map_err(|e| vec![e])?;
    if items.is_empty() {
        return Ok(0);
    }
    // SRD-46 routing — AUTOMATIC end-of-run render. This is the
    // path the workload governs: an item goes where its `to:`
    // says, and an item that declared nothing goes to the session
    // directory ONLY. stdout is never implied here, because a
    // run's stdout is its op output — appending a report that
    // carries wall-clock values would make that output
    // non-reproducible for anything comparing two runs.
    for it in &mut items {
        it.destinations = Some(
            it.destinations
                .clone()
                .unwrap_or_else(|| vec![nmbrs_workload::report::Destination::SessionDir]),
        );
    }
    let items = items;
    let output_root = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let strict = std::env::var("NMBRS_STRICT").is_ok();
    let failures = render_all(
        &items,
        KindFilter::Any,
        &[],
        None,
        &output_root,
        Some(db_path),
        strict,
    );
    if failures.is_empty() {
        Ok(items.len())
    } else {
        Err(failures)
    }
}

fn classify_render_error(e: String, strict: bool, failures: &mut Vec<String>) {
    let is_no_data = crate::plot_metrics::is_no_data_error(&e);
    let display = crate::plot_metrics::strip_no_data_prefix(&e);
    if is_no_data && !strict {
        eprintln!("WARNING: {display}");
    } else {
        eprintln!("ERROR: {display}");
        failures.push(display);
    }
}

// Selector + render context threaded explicitly; the sibling
// `render_by_*` helpers share this shape.
#[allow(clippy::too_many_arguments)]
fn render_by_index(
    items: &[ResolvedItem],
    filter: KindFilter,
    n_arg: &str,
    passthrough: &[String],
    workload_arg: Option<&str>,
    output_root: &Path,
    session_db: Option<&Path>,
    strict: bool,
) -> Vec<String> {
    let indices = match parse_figure_selector(n_arg) {
        Some(v) if !v.is_empty() => v,
        _ => {
            eprintln!(
                "nmbrs report figure: argument must be a positive integer, range, or list (got '{n_arg}')\n  \
                 accepted forms: `5`, `2-4`, `2..4`, `2..=4`, `1,3,5`, `1,3-5,7`"
            );
            std::process::exit(2);
        }
    };
    render_by_indices(
        items,
        filter,
        &indices,
        passthrough,
        workload_arg,
        output_root,
        session_db,
        strict,
    )
}

// Selector + render context threaded explicitly; the sibling
// `render_by_*` helpers share this shape.
#[allow(clippy::too_many_arguments)]
fn render_by_indices(
    items: &[ResolvedItem],
    filter: KindFilter,
    indices: &[usize],
    passthrough: &[String],
    workload_arg: Option<&str>,
    output_root: &Path,
    session_db: Option<&Path>,
    strict: bool,
) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    // Walk in given order so the user sees output in the
    // order they requested (`5,3,1` renders 5 then 3 then 1).
    for &n in indices {
        let Some(item) = items.get(n.saturating_sub(1)) else {
            eprintln!("nmbrs report: figure {n} out of range (1..{})", items.len());
            std::process::exit(2);
        };
        if !filter.matches(item.kind) {
            eprintln!(
                "nmbrs report: figure {n} is a {} but the kind filter requires {:?}",
                item.kind.as_str(),
                filter,
            );
            std::process::exit(2);
        }
        if let Err(e) = render_one(n, item, passthrough, workload_arg, output_root, session_db) {
            classify_render_error(e, strict, &mut failures);
        }
    }
    failures
}

/// Parse a figure-number selector into a list of 1-based
/// indices. Accepted forms (mix-and-match):
///
/// - `5` — single index
/// - `2-4` — inclusive range (hyphen)
/// - `2..4` — inclusive range (Rust-style; both `..` and `..=`
///   are inclusive in this CLI surface — the human-typing
///   convention overrides the Rust half-open convention here)
/// - `1,3,5` — explicit list
/// - `1,3-5,7` — list with embedded ranges
///
/// Returns `None` if any token isn't numeric / range-shaped.
/// Out-of-order ranges (`5-2`) error rather than treating as
/// reversed iteration. Empty input → `None`.
fn parse_figure_selector(s: &str) -> Option<Vec<usize>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut out: Vec<usize> = Vec::new();
    for token in s.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        // Try Rust-style range first so `2..4` doesn't get
        // hyphen-split. `..=` is the inclusive form; for this
        // CLI we treat `..` as inclusive too — humans typing
        // `2..4` usually mean "2 through 4."
        let (lo, hi) = if let Some((l, r)) = token.split_once("..=") {
            (l.trim(), Some(r.trim()))
        } else if let Some((l, r)) = token.split_once("..") {
            (l.trim(), Some(r.trim()))
        } else if let Some((l, r)) = token.split_once('-') {
            // `-` ambiguity: the empty-LHS case (`-5`) would be
            // a negative literal — figure indices are positive,
            // so reject it.
            if l.is_empty() {
                return None;
            }
            (l.trim(), Some(r.trim()))
        } else {
            (token, None)
        };
        let lo: usize = lo.parse().ok()?;
        if lo == 0 {
            return None;
        }
        match hi {
            Some(h) => {
                let hi: usize = h.parse().ok()?;
                if hi < lo {
                    return None;
                }
                for i in lo..=hi {
                    out.push(i);
                }
            }
            None => out.push(lo),
        }
    }
    Some(out)
}

// Selector + render context threaded explicitly; the sibling
// `render_by_*` helpers share this shape.
#[allow(clippy::too_many_arguments)]
fn render_by_glob(
    items: &[ResolvedItem],
    filter: KindFilter,
    glob: &str,
    passthrough: &[String],
    workload_arg: Option<&str>,
    output_root: &Path,
    session_db: Option<&Path>,
    strict: bool,
) -> Vec<String> {
    // Build (figure_num, item) pairs for figures that pass the
    // kind filter and the glob. Counter advances over every
    // figure in declaration order so the numbers stay stable.
    let mut fig_num: usize = 0;
    let mut matches: Vec<(usize, &ResolvedItem)> = Vec::new();
    for item in items.iter() {
        if item.kind.is_figure() {
            fig_num += 1;
        }
        if !filter.matches(item.kind) {
            continue;
        }
        if !glob_matches(glob, &item.name) {
            continue;
        }
        matches.push((fig_num, item));
    }
    if matches.is_empty() {
        eprintln!("nmbrs report: no items match '{glob}'");
        std::process::exit(2);
    }
    let mut failures: Vec<String> = Vec::new();
    for (n, item) in matches {
        if let Err(e) = render_one(n, item, passthrough, workload_arg, output_root, session_db) {
            classify_render_error(e, strict, &mut failures);
        }
    }
    failures
}

/// Render a single resolved item. Returns `Err(message)`
/// when a plot fails to render — the caller is responsible
/// for surfacing the failure and exiting nonzero. Plot
/// failures must not be silently dropped: a missing figure
/// is a real defect in the workload or its data, not a
/// recoverable condition (see "Never Ignore Silently"
/// guidance). Tables route through `summary_command`,
/// which currently exits the process on its own errors —
/// when that gets refactored to return Result, table
/// failures should funnel through the same path as plots.
fn render_one(
    n: usize,
    item: &ResolvedItem,
    passthrough: &[String],
    workload_arg: Option<&str>,
    output_root: &Path,
    session_db: Option<&Path>,
) -> Result<(), String> {
    use nmbrs_workload::report::Kind;
    // File items are scope directives — they don't render
    // anything themselves; their children do (during normal
    // iteration through the items list).
    if matches!(item.kind, Kind::File) {
        return Ok(());
    }
    if matches!(item.kind, Kind::Text) {
        render_text(item, output_root);
        return Ok(());
    }
    let mut base: Vec<String> = Vec::new();
    if let Some(w) = workload_arg {
        base.push(w.to_string());
    }
    // Re-inject the resolved session db as an explicit `--db`
    // so the downstream renderer (which sees only `base` plus
    // `passthrough`, neither containing the original `--session`
    // since `extract_workload` peeled it off) reads from the
    // user-named session and not from `logs/latest`.
    //
    // Skipped when the operator already supplied a `--db`, the same guard
    // `forward_renderer_flags` uses. Injecting a second one made the renderer
    // see TWO dbs, which routes through `db_merge` — so `session=<dir> --db
    // <same dir>/metrics.db` merged a database with itself, turning a
    // seconds-long render into a multi-minute one that looks like a hang.
    let passthrough_has_db = passthrough
        .iter()
        .any(|a| a == "--db" || a.starts_with("--db="));
    if let (Some(db), false) = (session_db, passthrough_has_db) {
        base.push("--db".into());
        base.push(db.to_string_lossy().into_owned());
    }
    base.push(format!("--name={}", item.name));
    base.push("--figure-num".into());
    base.push(n.to_string());
    // SRD-46 output routing. Forwarded only when the entry point
    // resolved a set (the automatic end-of-run path does; the
    // explicit command leaves it unset so the renderer default
    // applies). Skipped when the operator already passed `--to`,
    // the same guard `--db` uses, so an explicit override is
    // never shadowed by a second flag.
    let passthrough_has_to = passthrough
        .iter()
        .any(|a| a == "--to" || a.starts_with("--to="));
    if let (Some(dests), false) = (item.destinations.as_deref(), passthrough_has_to) {
        base.push("--to".into());
        base.push(
            dests
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    if let Some(l) = item.label.as_deref() {
        base.push("--label".into());
        base.push(l.to_string());
    }
    // Every figure upserts its section into a target markdown — the
    // declared `file` target, or the default `summary.md` the listing
    // footer advertises. (Previously only declared targets got the
    // `--report` upsert, so synthesized tables landed ONLY in their
    // standalone sidecars and summary.md held just the text sections.)
    let report_target = item.target_file.as_deref().unwrap_or("summary.md");
    base.push("--report".into());
    base.push(
        output_root
            .join(report_target)
            .to_string_lossy()
            .into_owned(),
    );
    // Plot-only style flags — appended only when forwarding to
    // the plot renderer. The summary (table) renderer doesn't
    // know `--palette` / `--line` / etc. and would mis-capture
    // their values as a positional spec.
    if matches!(item.kind, Kind::Plot) {
        if let Some(p) = item.palette.as_deref() {
            base.push("--palette".into());
            base.push(p.to_string());
        }
        if let Some(l) = item.line.as_deref() {
            base.push("--line".into());
            base.push(l.to_string());
        }
        if let Some(w) = item.width {
            base.push("--line-width".into());
            base.push(w.to_string());
        }
        if let Some(m) = item.marker.as_deref() {
            base.push("--marker".into());
            base.push(m.to_string());
        }
        if let Some(s) = item.marker_size {
            base.push("--marker-size".into());
            base.push(s.to_string());
        }
        // Per-series style overrides. One `--style` flag per
        // override, repeated. Value form is the brace-free
        // directive list `key=value:k=v k=v` so the renderer
        // can parse it back identically to how the YAML body
        // / CLI surface emit them.
        for so in &item.series_overrides {
            base.push("--style".into());
            let mut s = format!("{}={}:", so.key, so.value);
            let mut first = true;
            for line in so.style.scalar_directive_lines() {
                if !first {
                    s.push(' ');
                }
                first = false;
                s.push_str(&line);
            }
            base.push(s);
        }
    }
    base.extend(passthrough.iter().cloned());
    match item.kind {
        Kind::Plot => {
            // Use the result-returning variant so a no-rows
            // failure on one plot doesn't abort the rest of a
            // `report all` batch — but we surface the failure
            // to the caller so the overall report exits nonzero
            // when any figure failed. Silent skip would let a
            // broken workload masquerade as a successful run.
            //
            // Preserve the `[no-data]` sentinel from
            // `plot_metrics` through the wrap so the upstream
            // collector can downgrade it to a warning under
            // non-strict mode (incremental / auto-render
            // legitimately produces empty results before data
            // accumulates).
            let plot_result =
                crate::plot_metrics::plot_metrics_command_result(&base).map_err(|e| {
                    if crate::plot_metrics::is_no_data_error(&e) {
                        format!(
                            "{}plot '{}' has no data: {}",
                            crate::plot_metrics::PLOT_NO_DATA_PREFIX,
                            item.name,
                            crate::plot_metrics::strip_no_data_prefix(&e),
                        )
                    } else {
                        format!("plot '{}' failed: {e}", item.name)
                    }
                });

            // SRD-46 plot-only `with-table: true` companion.
            // Render the table view immediately after the
            // plot so the markdown carries both views in the
            // same section flow. The companion uses the same
            // body as the plot; the summary renderer reads
            // off `y / y1 / y2 / y3 / y4 / x-ticks` queries
            // and tabulates them. Failures here are
            // *secondary* — the plot's already rendered (or
            // already failed); the companion's outcome is
            // logged but doesn't replace the plot's
            // success/failure return.
            if plot_result.is_ok() {
                if item.with_table
                    && let Err(e) = render_companion_table(n, item, output_root, session_db, &[])
                {
                    eprintln!(
                        "WARNING: companion table for plot '{}' failed: {e}",
                        item.name,
                    );
                }
                if !item.with_tables.is_empty() {
                    match discover_faceted_tuples(item, session_db) {
                        Ok(tuples) if tuples.is_empty() => {
                            eprintln!(
                                "WARNING: with-tables for plot '{}' found no \
                                 distinct value tuples for labels {:?}",
                                item.name, item.with_tables,
                            );
                        }
                        Ok(tuples) => {
                            for tuple in tuples {
                                let pairs: Vec<(String, String)> = item
                                    .with_tables
                                    .iter()
                                    .cloned()
                                    .zip(tuple.iter().cloned())
                                    .collect();
                                if let Err(e) =
                                    render_companion_table(n, item, output_root, session_db, &pairs)
                                {
                                    eprintln!(
                                        "WARNING: faceted companion table for \
                                         plot '{}' ({pairs:?}) failed: {e}",
                                        item.name,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "WARNING: with-tables for plot '{}' could not \
                                 discover label tuples: {e}",
                                item.name,
                            );
                        }
                    }
                }
            }
            plot_result
        }
        Kind::Table => {
            // Standalone table naming convention:
            // `<item.name>_table.md`. Bypasses summary's
            // default `<name>_summary.<format>` suffix by
            // passing an explicit `--output`. Anchored at
            // `output_root` — the directory the preamble
            // advertises and the closing inventory diffs —
            // which is `<session>/report/` for synthesized
            // reports and the session root otherwise.
            // (Previously anchored at the db's directory,
            // which scattered sidecars outside the declared
            // output dir whenever the two differed.)
            let mut argv = base;
            let out = output_root.join(format!("{}_table.md", item.name));
            argv.push("--output".into());
            argv.push(out.to_string_lossy().into_owned());
            // Pass the item's own spec body, the way `plot_body_specs` does for
            // plots. Sending only `--name=` made the renderer look the name up
            // among the db's `summary.*` rows — which a `report:`-block item
            // does not have — so the render failed whenever the item list came
            // from anywhere but a `workload=` token. Carrying the body makes
            // the render depend on the resolved item alone, so a live session
            // and a finished one behave the same.
            if !item.body.trim().is_empty() {
                argv.push(item.body.clone());
            }
            // Report tables render whatever the session recorded:
            // a table whose phases didn't run has no rows, and
            // that must not abort the remaining items.
            argv.push("--empty-ok".into());
            crate::summary::summary_command(&argv);
            Ok(())
        }
        Kind::Text | Kind::File | Kind::Details => unreachable!(),
    }
}

/// Render a companion table for a plot whose body declared
/// `with-table: true`. Reuses the plot's body (each `y` /
/// `y1` / `y2` / `y3` / `y4` line becomes one table column;
/// `x` / `x-ticks` provide the row key). The table writes
/// into the same target markdown file as the plot,
/// immediately after the plot's section, with an anchor
/// derived from the plot's name plus a `_table` suffix so
/// users can link to either view independently.
///
/// Implemented as a thin facade over the existing summary
/// renderer: we synthesise a table-shaped argv from the
/// plot's `y*` queries, call `summary_command`, and let it
/// do the markdown emission via the same path tables use
/// today.
fn render_companion_table(
    plot_figure_num: usize,
    item: &ResolvedItem,
    output_root: &Path,
    session_db: Option<&Path>,
    facet: &[(String, String)],
) -> Result<(), String> {
    let columns = extract_y_queries(&item.body);
    if columns.is_empty() {
        return Err("no y/y1/y2/y3/y4 query lines in plot body".into());
    }
    // When faceting, inject the (label=value) constraints
    // into every column expression so each table sees only
    // rows that match this facet. Removes the facet labels
    // from group_by since they're now constant per table.
    let columns: Vec<(String, String)> = if facet.is_empty() {
        columns
    } else {
        columns
            .into_iter()
            .map(|(name, q)| (name, inject_label_matchers(&q, facet)))
            .collect()
    };
    // Group-by: union of every discriminator the plot
    // actually breaks series on. Otherwise the table
    // collapses dimensions the plot keeps separate (e.g.
    // averaging `optimize_for` away when each plot line
    // shows a distinct value). Sources, in order:
    //   1. The `x:` value when it's a bare label name —
    //      the per-row identity in the plot.
    //   2. Every `by (k1, k2, …)` clause across x-ticks
    //      and every y* query.
    let mut group_by: Vec<String> = Vec::new();
    let push_unique = |k: String, gb: &mut Vec<String>| {
        if !k.is_empty() && !gb.iter().any(|e| e == &k) {
            gb.push(k);
        }
    };
    if let Some(x) = extract_x_query(&item.body) {
        let x_trim = x.trim();
        // Bare label form: identifier with no whitespace / parens.
        if !x_trim.is_empty()
            && !x_trim.contains(|c: char| c.is_whitespace() || c == '(' || c == '{')
        {
            push_unique(x_trim.to_string(), &mut group_by);
        } else {
            for k in extract_label_keys(&x) {
                push_unique(k, &mut group_by);
            }
        }
    }
    if let Some(xt) = extract_xticks_query(&item.body) {
        for k in extract_label_keys(&xt) {
            push_unique(k, &mut group_by);
        }
    }
    for (_col, q) in &columns {
        for k in extract_label_keys(q) {
            push_unique(k, &mut group_by);
        }
    }
    // Faceting fixes those labels to constant values for
    // this table; pulling them out of group_by avoids
    // single-value columns that just repeat the facet.
    if !facet.is_empty() {
        let facet_keys: std::collections::HashSet<&str> =
            facet.iter().map(|(k, _)| k.as_str()).collect();
        group_by.retain(|k| !facet_keys.contains(k.as_str()));
    }

    // Encode columns + group_by into a summary spec string
    // (`query <col>: <expr>` / `group_by: <keys>`). The summary
    // parser already consumes this form natively (SRD-46 v2),
    // so the companion-table feature reuses one DSL instead of
    // a parallel flag surface.
    let mut spec = String::new();
    if !group_by.is_empty() {
        spec.push_str(&format!("group_by: {}\n", group_by.join(",")));
    }
    for (col_name, query) in columns {
        spec.push_str(&format!("query {col_name}: {query}\n"));
    }

    let plot_label = item
        .label
        .clone()
        .unwrap_or_else(|| crate::report::prettify_name(&item.name));
    // Facet suffix folded into both the markdown section
    // name (so each table gets its own anchor / file slot)
    // and the heading label (so the operator can see which
    // slice they're looking at).
    // Companion-table naming convention:
    //   non-faceted: `<plot>_table.md`
    //   faceted:     `<plot>__<key>_<value>[__<key2>_<value2>...].md`
    // (matches the user-facing rule in
    //  docs/SRD/46_reports.md / per workload guidance.)
    let (basename, name_arg) = if facet.is_empty() {
        let stem = format!("{}_table", item.name);
        (stem.clone(), stem)
    } else {
        let mut s = item.name.clone();
        for (k, v) in facet {
            s.push_str("__");
            s.push_str(&sanitize_for_anchor(k));
            s.push('_');
            s.push_str(&sanitize_for_anchor(v));
        }
        (s.clone(), s)
    };
    let facet_suffix_label = if facet.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = facet.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!(" [{}]", parts.join(", "))
    };
    let mut argv: Vec<String> = vec![
        format!("--name={name_arg}"),
        "--figure-num".into(),
        plot_figure_num.to_string(),
        "--label".into(),
        format!("{plot_label} (data){facet_suffix_label}"),
    ];
    if let Some(target) = item.target_file.as_deref() {
        argv.push("--report".into());
        argv.push(output_root.join(target).to_string_lossy().into_owned());
    }
    if let Some(db) = session_db {
        argv.push("--db".into());
        argv.push(db.to_string_lossy().into_owned());
    }
    // Explicit output path — overrides summary's default
    // `<basename>_summary.md` so the on-disk file matches
    // the prescribed name. Anchored at the db's directory
    // when known, otherwise at `output_root` (which itself
    // resolves to `logs/latest` for the default `nmbrs report`
    // flow).
    let out_dir = session_db
        .and_then(|d| d.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| output_root.to_path_buf());
    let out = out_dir.join(format!("{basename}.md"));
    argv.push("--output".into());
    argv.push(out.to_string_lossy().into_owned());
    argv.push(spec);
    argv.push("--empty-ok".into());
    crate::summary::summary_command(&argv);
    Ok(())
}

/// Inject `key="value"` matchers into the first metric
/// selector in a metricsql expression. Used to scope each
/// faceted companion table to a single (label=value) tuple
/// without re-implementing metricsql parsing.
///
/// Targets the first `{ … }` block. Inserts before the
/// closing brace (or replaces an empty `{}` with the
/// matchers). When no `{ … }` exists, wraps the bare
/// metric name with one.
fn inject_label_matchers(expr: &str, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return expr.to_string();
    }
    let inj: String = pairs
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    if let Some(open) = expr.find('{') {
        // Find the matching close at depth 0 in label space.
        let after_open = &expr[open + 1..];
        if let Some(close_rel) = after_open.find('}') {
            let close = open + 1 + close_rel;
            let inner = expr[open + 1..close].trim();
            let mut out = String::with_capacity(expr.len() + inj.len() + 2);
            out.push_str(&expr[..open + 1]);
            if !inner.is_empty() {
                out.push_str(inner);
                out.push(',');
            }
            out.push_str(&inj);
            out.push_str(&expr[close..]);
            return out;
        }
    }
    // No selector — bolt one on at the end of the bare
    // metric name. Heuristic: drop a `{inj}` right after the
    // first identifier we can find.
    let mut chars = expr.chars().enumerate().peekable();
    let mut ident_end: Option<usize> = None;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_alphanumeric() || c == '_' || c == ':' {
            chars.next();
            ident_end = Some(i + c.len_utf8());
        } else if ident_end.is_some() {
            break;
        } else {
            chars.next();
        }
    }
    match ident_end {
        Some(end) => format!("{}{{{inj}}}{}", &expr[..end], &expr[end..]),
        None => expr.to_string(),
    }
}

/// Reduce a label value to a token usable in an anchor /
/// filename suffix: keep alphanumerics + `_`, replace
/// everything else with `_`.
fn sanitize_for_anchor(v: &str) -> String {
    v.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Discover the distinct value tuples for the given label
/// keys by running the plot's first metricsql query against
/// the session db and gathering each result series's labels.
/// Returns one `Vec<String>` per distinct tuple, with the
/// values in the same order as `item.with_tables`.
fn discover_faceted_tuples(
    item: &ResolvedItem,
    session_db: Option<&Path>,
) -> Result<Vec<Vec<String>>, String> {
    use std::collections::BTreeSet;
    let db_path = match session_db {
        Some(p) => p.to_path_buf(),
        None => nmbrs_runtime::session::latest_metrics_db(),
    };
    if !db_path.exists() {
        return Err(format!("session db '{}' missing", db_path.display()));
    }
    let columns = extract_y_queries(&item.body);
    let first = columns
        .first()
        .ok_or_else(|| "plot has no y queries — nothing to facet over".to_string())?;
    let expr = &first.1;

    use nmbrs_metrics::queryapi::sqlite::SqliteDataSource;
    use nmbrs_metricsql::eval::{EvalContext, evaluate};
    let ds = SqliteDataSource::open(&db_path)
        .map_err(|e| format!("open metricsql sqlite adapter: {e}"))?;
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| format!("open db: {e}"))?;
    let (min_ts, max_ts): (i64, i64) = conn.query_row(
        "SELECT COALESCE(MIN(timestamp_ms), 0), COALESCE(MAX(timestamp_ms), 0) FROM sample_value",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(|e| format!("read time bounds: {e}"))?;
    if max_ts == 0 {
        return Ok(Vec::new());
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
    let parsed = nmbrs_metricsql::parse(expr).map_err(|e| format!("parse '{expr}': {e}"))?;
    // SRD-77 — facet discovery coalesces across executions
    // (per-instance-latest, the DataSource default), so a refined
    // session's companion tables break down by every facet, not just
    // the newest execution's.
    let series = evaluate(&ctx, &parsed).map_err(|e| format!("evaluate '{expr}': {e}"))?;
    let mut seen: BTreeSet<Vec<String>> = BTreeSet::new();
    for s in series {
        let tuple: Vec<String> = item
            .with_tables
            .iter()
            .map(|k| {
                s.labels
                    .iter()
                    .find(|(lk, _)| lk == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            })
            .collect();
        if tuple.iter().all(|v| !v.is_empty()) {
            seen.insert(tuple);
        }
    }
    Ok(seen.into_iter().collect())
}

/// Pull every `y[N]: <query>` line out of a plot body,
/// returning `(column_name, query)` pairs. The column name
/// is sourced from the plot's legend declarations so the
/// companion table's headers mirror the plot legend:
///   1. Per-axis `yN-legend:` template (singular) wins.
///   2. Positional `y-legends: [t1, t2, t3]` (axis index).
///   3. Bare axis tag (`y1` / `y2` / …) when no legend
///      template is declared.
///
/// `[placeholder]` tokens (e.g. `[optimize_for]`) are
/// stripped — the table already breaks down by those labels
/// in their own columns, so leaving the placeholder text
/// would make headers noisier than they need to be.
fn extract_y_queries(body: &str) -> Vec<(String, String)> {
    let axis_tag = |prefix: &str| -> &'static str {
        match prefix {
            "y:" | "y1:" => "y1",
            "y2:" => "y2",
            "y3:" => "y3",
            "y4:" => "y4",
            _ => unreachable!(),
        }
    };
    // Pre-scan for legend declarations.
    let mut per_axis_legend: std::collections::HashMap<&'static str, String> =
        std::collections::HashMap::new();
    let mut positional: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        for pfx in [
            "y-legend:",
            "y1-legend:",
            "y2-legend:",
            "y3-legend:",
            "y4-legend:",
        ] {
            if let Some(rest) = line.strip_prefix(pfx) {
                let key = match pfx {
                    "y-legend:" | "y1-legend:" => "y1",
                    "y2-legend:" => "y2",
                    "y3-legend:" => "y3",
                    "y4-legend:" => "y4",
                    _ => unreachable!(),
                };
                per_axis_legend.insert(key, strip_outer_quotes(rest.trim()).to_string());
            }
        }
        if let Some(rest) = line.strip_prefix("y-legends:") {
            // `[a, b, "c d", …]` — same shape as the plot
            // parser's `split_array_value`. Quoted entries
            // keep their inner whitespace; unquoted bare
            // tokens are trimmed.
            let trimmed = rest.trim();
            if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                positional = split_legend_array(inner);
            }
        }
    }
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        for prefix in ["y:", "y1:", "y2:", "y3:", "y4:"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let key = axis_tag(prefix);
                let axis_idx: usize = match key {
                    "y1" => 0,
                    "y2" => 1,
                    "y3" => 2,
                    "y4" => 3,
                    _ => 0,
                };
                let col_name = per_axis_legend
                    .get(key)
                    .cloned()
                    .or_else(|| positional.get(axis_idx).cloned())
                    .unwrap_or_else(|| key.to_string());
                let col_name = strip_legend_placeholders(&col_name);
                // Compact pair shorthand (`(M_x, M_y[, *|label]){...}
                // by (...)`) isn't valid metricsql — decompose to
                // the underlying y query before exposing the value
                // to downstream metricsql parsers (e.g. the
                // `with-tables` label-tuple discovery in
                // discover_faceted_tuples). When the value isn't a
                // shorthand, pass through verbatim.
                let raw = rest.trim().to_string();
                let y_query = crate::plot_metrics::try_decompose_y_query(&raw).unwrap_or(raw);
                out.push((col_name, y_query));
                break;
            }
        }
    }
    out
}

/// Strip a single layer of `"…"` or `'…'` from a token.
fn strip_outer_quotes(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        .unwrap_or(s)
}

/// Drop `[placeholder]` tokens from a legend template — the
/// companion table breaks down by those labels in their own
/// columns, so the placeholder text in the header would just
/// duplicate that information.
fn strip_legend_placeholders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth: i32 = 0;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // Collapse the residual whitespace / dangling separators
    // left by the placeholder removal.
    let cleaned = out
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");
    if cleaned.is_empty() {
        "value".to_string()
    } else {
        cleaned
    }
}

/// Split `y-legends:` array contents on top-level commas,
/// preserving quoted entries verbatim (sans outer quotes).
fn split_legend_array(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    let mut depth: i32 = 0;
    let push = |buf: &mut String, out: &mut Vec<String>| {
        let trimmed = strip_outer_quotes(buf.trim()).to_string();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
        buf.clear();
    };
    for c in inner.chars() {
        if let Some(q) = in_quote {
            current.push(c);
            if c == q {
                in_quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_quote = Some(c);
                current.push(c);
            }
            '[' | '(' => {
                depth += 1;
                current.push(c);
            }
            ']' | ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => push(&mut current, &mut out),
            _ => current.push(c),
        }
    }
    push(&mut current, &mut out);
    out
}

/// Pull the `x: <query>` line if present.
fn extract_x_query(body: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("x:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Pull the `x-ticks: <query>` line if present.
fn extract_xticks_query(body: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("x-ticks:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Pull the label keys out of a `... by (k1, k2, k3)` clause
/// in a metricsql query. Used to seed the companion
/// table's group_by so each row corresponds to one (k1,
/// k2, k3) tuple of the plot's series discriminators.
fn extract_label_keys(query: &str) -> Vec<String> {
    let lower = query.to_ascii_lowercase();
    let by_idx = lower.rfind(" by ");
    let by_idx = match by_idx {
        Some(i) => i,
        None => return Vec::new(),
    };
    let after = &query[by_idx + 4..];
    let after = after.trim();
    let inner = after
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(after);
    inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Render a text item by writing its body verbatim into the
/// target markdown file (or `summary.md` when no `target_file`
/// is set). The heading uses the label, falling back to a
/// prettified canonical name. No figure number — text isn't a
/// figure (SRD-46).
fn render_text(item: &ResolvedItem, output_root: &Path) {
    let target = item.target_file.as_deref().unwrap_or("summary.md");
    let path = output_root.join(target);
    let label = item
        .label
        .clone()
        .unwrap_or_else(|| crate::report::prettify_name(&item.name));
    let heading_display = format!("{label} (text)");
    if let Err(e) = crate::report::write_named_section(
        &path,
        &item.name,
        &heading_display,
        &item.body,
        crate::report::WriteMode::Update,
    ) {
        eprintln!(
            "warning: failed to write text section to '{}': {e}",
            path.display()
        );
    }
}

/// Tiny glob matcher: supports `*` (any), `?` (any one char).
/// `[abc]` brackets and brace expansion are out of scope —
/// add only when an example workload needs them.
fn glob_matches(glob: &str, name: &str) -> bool {
    fn rec(g: &[u8], n: &[u8]) -> bool {
        match (g.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if rec(&g[1..], n) {
                    return true;
                }
                if !n.is_empty() && rec(g, &n[1..]) {
                    return true;
                }
                false
            }
            (Some(b'?'), Some(_)) => rec(&g[1..], &n[1..]),
            (Some(gc), Some(nc)) if gc == nc => rec(&g[1..], &n[1..]),
            _ => false,
        }
    }
    rec(glob.as_bytes(), name.as_bytes())
}

// ── cli_spec entry ─────────────────────────────────────────

/// Build a child Command for one report subcommand. Every
/// child is `raw_args=true` because its parser is owned by
/// `report_command` (vocab-driven for plot/table/etc.,
/// hand-rolled for list/all/show/figure/rename/scratch).
/// Handler reconstructs `[subname, ...raw]` and forwards to
/// the dispatcher so the legacy parser sees its expected
/// shape.
fn report_subleaf(subname: &'static str, help: &'static str) -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    // One-off handlers per subname — fn pointers can't capture
    // so we route through a shared dispatch table by name.
    fn h(subname: &'static str) -> fn(ParsedCommand) -> Result<(), String> {
        match subname {
            "plot" => |p| {
                dispatch(p, "plot");
                Ok(())
            },
            "table" => |p| {
                dispatch(p, "table");
                Ok(())
            },
            "text" => |p| {
                dispatch(p, "text");
                Ok(())
            },
            "file" => |p| {
                dispatch(p, "file");
                Ok(())
            },
            "details" => |p| {
                dispatch(p, "details");
                Ok(())
            },
            "list" => |p| {
                dispatch(p, "list");
                Ok(())
            },
            "all" => |p| {
                dispatch(p, "all");
                Ok(())
            },
            "show" => |p| {
                dispatch(p, "show");
                Ok(())
            },
            "figure" => |p| {
                dispatch(p, "figure");
                Ok(())
            },
            "rename" => |p| {
                dispatch(p, "rename");
                Ok(())
            },
            "scratch" => |p| {
                dispatch(p, "scratch");
                Ok(())
            },
            "synth" => |p| {
                dispatch(p, "synth");
                Ok(())
            },
            _ => |_| Err("report: unknown subcommand".to_string()),
        }
    }
    fn dispatch(p: ParsedCommand, subname: &str) {
        let mut argv: Vec<String> = vec![subname.to_string()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
    }
    // Common selection flags for every report subcommand; `show`
    // additionally completes stored item names on `--name`.
    let mut flags = vec![
        crate::cli_spec::Flag {
            long: "--db",
            short: None,
            aliases: &[],
            arity: crate::cli_spec::Arity::Value,
            value: crate::cli_spec::ValueProvider::Path,
            help: "Metrics database path (default: logs/latest/metrics.db).",
            repeatable: false,
        },
        crate::cli_spec::Flag {
            long: "--session",
            short: None,
            aliases: &[],
            arity: crate::cli_spec::Arity::Value,
            value: crate::completion::SESSION_NAME_VALUE,
            help: "Session name or path.",
            repeatable: false,
        },
        crate::completion::workload_flag("Workload file providing the report: block."),
    ];
    if subname == "show" {
        flags.push(crate::cli_spec::Flag {
            long: "--name",
            short: None,
            aliases: &[],
            arity: crate::cli_spec::Arity::Value,
            value: crate::cli_spec::ValueProvider::Custom(
                crate::completion::report_any_name_provider,
            ),
            help: "Stored report item name.",
            repeatable: false,
        });
    }
    Command {
        name: subname,
        help,
        category: Category::Tools,
        level: Level::Secondary,
        flags,
        kv_params: crate::completion::REPORT_KV,
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(h(subname))),
        raw_args: true,
        completion_override: None,
    }
}

/// `nmbrs report …` — SRD-46/64 report surface. raw_args at
/// each leaf because the existing parser is vocab-driven and
/// richer than the generic walker can express today. The spec
/// declares every subcommand so tab on `nmbrs report <TAB>`
/// surfaces them; handlers reconstruct the legacy argv shape
/// and dispatch.
///
/// **Open gap:** vocab-driven *flag* completion under each
/// kind leaf (e.g. `nmbrs report plot --<TAB>`) is still
/// served by the legacy `kind_subcommand_node()` in
/// `completion.rs` because cli_spec's Flag model doesn't yet
/// import vocab Directives. Future work: a
/// `vocab::Directive → cli_spec::Flag` adapter so the spec
/// becomes the only flag-source.
pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    // Group-level handler covers the "no subcommand matched"
    // path: bare `nmbrs report` (lists figures), `nmbrs report
    // <glob>` (matches items by name), and pass-through for
    // unknown args. `raw_args: true` lets the walker hand
    // every remaining token to this handler verbatim — the
    // legacy `report_command` does the dispatch.
    fn handle(p: ParsedCommand) -> Result<(), String> {
        report_command(&p.raw, KindFilter::Any);
        Ok(())
    }
    Command {
        name: "report",
        help: "Render report items defined in a workload's `report:` block.",
        category: Category::Tools,
        level: Level::Secondary,
        // Declared even though `raw_args` skips the walker's parse:
        // help/completion advertise from here, and the dispatcher's
        // closed-surface check (`crate::completion::known_flags`)
        // derives from this spec — an undeclared flag is invisible
        // to all three.
        flags: vec![
            crate::cli_spec::Flag {
                long: "--synthesized",
                short: None,
                aliases: &[],
                arity: crate::cli_spec::Arity::Bool,
                value: crate::cli_spec::ValueProvider::None,
                help: "Render the SRD-109 synthesized section even when an \
                       explicit `report:` block exists.",
                repeatable: false,
            },
            crate::cli_spec::Flag {
                long: "--rebuild",
                short: None,
                aliases: &[],
                arity: crate::cli_spec::Arity::Bool,
                value: crate::cli_spec::ValueProvider::None,
                help: "Wipe declared target markdown files before rendering.",
                repeatable: false,
            },
            crate::cli_spec::Flag {
                long: "--clean",
                short: None,
                aliases: &[],
                arity: crate::cli_spec::Arity::Bool,
                value: crate::cli_spec::ValueProvider::None,
                help: "Remove rendered report outputs and exit.",
                repeatable: false,
            },
        ],
        kv_params: crate::completion::REPORT_KV,
        dynamic_options: None,
        positionals: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
        subcommands: vec![
            // Vocab-driven kind subcommands route their
            // *completion* through the legacy kind_subcommand_node
            // helper (which iterates `vocab::ALL_DIRECTIVES` to
            // produce flag completions + per-flag value
            // providers). The cli_spec adapter honors
            // `completion_override` and uses that Node verbatim.
            //
            // Only `plot` and `table` are advertised here:
            // the legacy parser only kind-promotes those two
            // (`text`/`file`/`details` exist in the workload
            // YAML grammar but `nmbrs report <kind>` doesn't
            // accept them on the CLI today). Surfacing them
            // would mislead users — the bare-form glob path
            // would error with "no items match".
            kind_subleaf(
                "plot",
                "Render plot items by name (kind-filtered).",
                nmbrs_workload::report::Kind::Plot,
            ),
            kind_subleaf(
                "table",
                "Render table items by name (kind-filtered).",
                nmbrs_workload::report::Kind::Table,
            ),
            // `list` and bare-form `nmbrs report` produce the
            // same listing — the user's canonical command is
            // `nmbrs report list`, the bare form is the shortcut.
            report_subleaf("list", "List figures defined in the report."),
            report_subleaf("all", "Render every report item."),
            report_subleaf("show", "Render one stored item by name."),
            report_subleaf("figure", "Render by figure number / range."),
            report_subleaf("synth", "Dump the SRD-109 synthesized report: block."),
            rename_subleaf(),
            scratch_subleaf(),
        ],
    }
}

/// Build a kind subcommand (`plot` / `table` / `text` / `file` /
/// `details`) whose completion routes through
/// [`crate::completion::kind_subcommand_node`] for vocab-driven
/// flag + value-provider plumbing.
fn kind_subleaf(
    subname: &'static str,
    help: &'static str,
    kind: nmbrs_workload::report::Kind,
) -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle_plot(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = vec!["plot".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn handle_table(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = vec!["table".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn handle_text(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = vec!["text".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn handle_file(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = vec!["file".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn handle_details(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = vec!["details".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    let handler = match subname {
        "plot" => Handler::Sync(handle_plot),
        "table" => Handler::Sync(handle_table),
        "text" => Handler::Sync(handle_text),
        "file" => Handler::Sync(handle_file),
        "details" => Handler::Sync(handle_details),
        _ => unreachable!(),
    };
    let override_fn: fn() -> veks_completion::Node = match kind {
        nmbrs_workload::report::Kind::Plot => {
            || crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::Plot)
        }
        nmbrs_workload::report::Kind::Table => {
            || crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::Table)
        }
        nmbrs_workload::report::Kind::Text => {
            || crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::Text)
        }
        nmbrs_workload::report::Kind::File => {
            || crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::File)
        }
        nmbrs_workload::report::Kind::Details => {
            || crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::Details)
        }
    };
    Command {
        name: subname,
        help,
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(handler),
        raw_args: true,
        completion_override: Some(override_fn),
    }
}

/// `nmbrs report rename` — typed flag set declared in
/// cli_spec so completion offers `--replace`, `--dry-run`,
/// `--workload`.
fn rename_subleaf() -> crate::cli_spec::Command {
    use crate::cli_spec::{
        Arity, Category, Command, Flag, Handler, Level, ParsedCommand, ValueProvider,
    };
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = vec!["rename".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    Command {
        name: "rename",
        help: "Rename a workload report item.",
        category: Category::Tools,
        level: Level::Secondary,
        flags: vec![
            crate::completion::workload_flag("Override the workload file to mutate."),
            Flag {
                long: "--replace",
                short: None,
                aliases: &[],
                arity: Arity::Bool,
                value: ValueProvider::None,
                help: "Overwrite if `<new>` already exists.",
                repeatable: false,
            },
            Flag {
                long: "--dry-run",
                short: None,
                aliases: &[],
                arity: Arity::Bool,
                value: ValueProvider::None,
                help: "Print intended change without writing.",
                repeatable: false,
            },
        ],
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

/// `nmbrs report scratch` — has its own list/clean/promote
/// children. Modelled as a Command group with raw_args leaves.
fn scratch_subleaf() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn h_list(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["scratch".into(), "list".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn h_clean(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["scratch".into(), "clean".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn h_promote(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["scratch".into(), "promote".into()];
        argv.extend(p.raw.iter().cloned());
        report_command(&argv, KindFilter::Any);
        Ok(())
    }
    fn child(
        name: &'static str,
        help: &'static str,
        handler: fn(ParsedCommand) -> Result<(), String>,
    ) -> Command {
        Command {
            name,
            help,
            category: Category::Tools,
            level: Level::Secondary,
            flags: Vec::new(),
            kv_params: &[],
            dynamic_options: None,
            positionals: Vec::new(),
            subcommands: Vec::new(),
            handler: Some(Handler::Sync(handler)),
            raw_args: true,
            completion_override: None,
        }
    }
    Command {
        name: "scratch",
        help: "Inspect / clean / promote scratch renders.",
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        handler: None,
        raw_args: false,
        completion_override: None,
        subcommands: vec![
            child("list", "List scratch entries.", h_list),
            child("clean", "Remove scratch entries.", h_clean),
            child("promote", "Promote scratch to workload.", h_promote),
        ],
    }
}

/// `nmbrs plot …` — unadvertised alias for `nmbrs report plot …`.
pub fn plot_alias_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        report_command(&p.raw, KindFilter::Plot);
        Ok(())
    }
    Command {
        name: "plot",
        help: "Alias for `nmbrs report plot`.",
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        // Same completion node as the real subcommand. Without this the
        // alias — the spelling most operators actually type — completed
        // NOTHING: not its flags, not `session=`, not report names.
        completion_override: Some(|| {
            crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::Plot)
        }),
    }
}

/// `nmbrs table …` — unadvertised alias for `nmbrs report table …`.
pub fn table_alias_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        report_command(&p.raw, KindFilter::Table);
        Ok(())
    }
    Command {
        name: "table",
        help: "Alias for `nmbrs report table`.",
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        // Same completion node as the real subcommand. Without this the
        // alias — the spelling most operators actually type — completed
        // NOTHING: not its flags, not `session=`, not report names.
        completion_override: Some(|| {
            crate::completion::kind_subcommand_node(nmbrs_workload::report::Kind::Table)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_wrapping_breaks_at_column_names_only() {
        let cols: Vec<String> = (0..12).map(|i| format!("col_{i:02}")).collect();
        let text = format!("by part: {}", cols.join(", "));
        let wrapped = wrap_hint_to(&text, 8, 60);
        for line in wrapped.split('\n') {
            // Content after the alignment indent fits the available
            // width (+1 for a wrap-point trailing comma).
            assert!(
                line.trim_start().chars().count() <= 53,
                "line overruns width: {line:?}"
            );
        }
        // Every column name survives intact — no mid-name breaks.
        for c in &cols {
            assert!(wrapped.contains(c.as_str()));
        }
        // Continuation lines align under the hint column.
        assert!(
            wrapped.contains("\n        col_"),
            "aligned continuation: {wrapped}"
        );
        // Short hints stay single-line.
        assert_eq!(wrap_hint_to("by phase: a, b", 8, 60), "by phase: a, b");
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// `--db` names a session as surely as `--session` does. Read only from
    /// `--session*` before, so a `--db` invocation resolved its item list and
    /// wrote its output under `sessions/latest` while the renderer read data from
    /// the named db — two sessions in one command, item list from the wrong one.
    #[test]
    fn db_flag_supplies_the_session_anchor() {
        assert_eq!(
            db_flag_path(&v(&["--db", "/tmp/s/metrics.db"])),
            Some(PathBuf::from("/tmp/s/metrics.db"))
        );
        assert_eq!(
            db_flag_path(&v(&["--db=/tmp/s/metrics.db"])),
            Some(PathBuf::from("/tmp/s/metrics.db"))
        );
        // Comma list: the first entry is primary, matching how the renderer
        // anchors output when it merges several dbs.
        assert_eq!(
            db_flag_path(&v(&["--db=/tmp/a.db,/tmp/b.db"])),
            Some(PathBuf::from("/tmp/a.db"))
        );
        // A db need not be named `metrics.db` — the path is carried as given
        // rather than rebuilt from the parent directory.
        assert_eq!(
            db_flag_path(&v(&["--db", "/tmp/s/custom.db"])),
            Some(PathBuf::from("/tmp/s/custom.db"))
        );
        assert_eq!(db_flag_path(&v(&["--name=x"])), None);
        // Trailing `--db` with no value must not panic or invent a path.
        assert_eq!(db_flag_path(&v(&["--db"])), None);
    }

    /// `refresh_session_index` writes `index.md` listing every
    /// non-skipped file in the directory, organised by category,
    /// with image entries embedded as previews.
    #[test]
    fn refresh_session_index_writes_categorised_links() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let dir = tmp.path();
        std::fs::write(dir.join("report_a.md"), "# A").unwrap();
        std::fs::write(dir.join("results.csv"), "k,v\n1,2\n").unwrap();
        std::fs::write(dir.join("plot.png"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("session.log"), "log").unwrap();
        std::fs::write(dir.join("metrics.db"), [0u8; 8]).unwrap();
        std::fs::write(dir.join("README"), "no ext").unwrap();
        // Skipped: index.md, dotfile, lockfile
        std::fs::write(dir.join("index.md"), "stale").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(dir.join("metrics.db.lock"), "x").unwrap();

        refresh_session_index(dir).expect("refresh");
        let body = std::fs::read_to_string(dir.join("index.md")).expect("read index");
        assert!(body.contains("## Reports"), "reports section missing");
        assert!(body.contains("[`report_a.md`](report_a.md)"));
        assert!(body.contains("## Figures"), "figures section missing");
        assert!(body.contains("[`plot.png`](plot.png)"));
        assert!(
            body.contains("![plot.png](plot.png)"),
            "image preview missing"
        );
        assert!(body.contains("## Tables / data"));
        assert!(body.contains("[`results.csv`](results.csv)"));
        assert!(body.contains("## Logs"));
        assert!(body.contains("[`session.log`](session.log)"));
        assert!(body.contains("## Database"));
        assert!(body.contains("[`metrics.db`](metrics.db)"));
        assert!(body.contains("## Other"));
        assert!(body.contains("[`README`](README)"));
        assert!(!body.contains("`.hidden`"));
        assert!(!body.contains(".lock`]"));
        let index_links = body.matches("[`index.md`]").count();
        assert_eq!(index_links, 0, "index.md should not list itself");
    }

    /// Empty directory still produces a valid index.
    #[test]
    fn refresh_session_index_empty_directory_writes_placeholder() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        refresh_session_index(tmp.path()).expect("refresh");
        let body = std::fs::read_to_string(tmp.path().join("index.md")).expect("read index");
        assert!(
            body.contains("no artifacts"),
            "empty-directory placeholder missing: {body}"
        );
    }

    /// Re-running on a populated directory overwrites cleanly —
    /// stale entries from a prior run don't linger.
    #[test]
    fn refresh_session_index_overwrites_stale_entries() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let dir = tmp.path();
        std::fs::write(dir.join("first.md"), "1").unwrap();
        refresh_session_index(dir).expect("first refresh");
        let body1 = std::fs::read_to_string(dir.join("index.md")).unwrap();
        assert!(body1.contains("first.md"));

        std::fs::remove_file(dir.join("first.md")).unwrap();
        std::fs::write(dir.join("second.md"), "2").unwrap();
        refresh_session_index(dir).expect("second refresh");
        let body2 = std::fs::read_to_string(dir.join("index.md")).unwrap();
        assert!(body2.contains("second.md"), "new entry missing");
        assert!(!body2.contains("first.md"), "stale entry persisted");
    }

    #[test]
    fn clean_wipe_removes_png_and_md_leaves_other_files() {
        // Mirrors the operator's intent for `--clean all`:
        // every `.png` and `.md` in the session dir is gone
        // post-wipe; non-artifact files (metrics.db, log,
        // checkpoint, etc.) survive. Subdirectories are
        // left alone — wipe is non-recursive.
        let dir = std::env::temp_dir().join(format!("nmbrs_clean_wipe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Artifact files (should be removed).
        std::fs::write(dir.join("recall_10_mean_plot.png"), b"PNG").unwrap();
        std::fs::write(dir.join("throughput_1__optimize_for_recall.md"), b"# tbl").unwrap();
        std::fs::write(dir.join("summary.md"), b"# top").unwrap();
        // Non-artifact files (should survive).
        std::fs::write(dir.join("metrics.db"), b"sqlite").unwrap();
        std::fs::write(dir.join("session.log"), b"log").unwrap();
        std::fs::write(dir.join("checkpoint.jsonl"), b"{}").unwrap();
        // Subdirectory (should survive untouched).
        std::fs::create_dir_all(dir.join("metrics")).unwrap();
        std::fs::write(dir.join("metrics/nested.md"), b"# nested").unwrap();

        clean_wipe_artifacts(&dir);

        assert!(
            !dir.join("recall_10_mean_plot.png").exists(),
            "png should be removed"
        );
        assert!(
            !dir.join("throughput_1__optimize_for_recall.md").exists(),
            "companion-table md should be removed"
        );
        assert!(
            !dir.join("summary.md").exists(),
            "summary.md should be removed"
        );
        assert!(dir.join("metrics.db").exists(), "metrics.db must survive");
        assert!(dir.join("session.log").exists(), "session.log must survive");
        assert!(
            dir.join("checkpoint.jsonl").exists(),
            "checkpoint.jsonl must survive"
        );
        assert!(
            dir.join("metrics/nested.md").exists(),
            "nested md inside subdir must survive (non-recursive wipe)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_clean_mode_recognises_flag_and_env() {
        assert!(is_clean_mode(&["--clean".to_string()]));
        assert!(is_clean_mode(&[
            "all".to_string(),
            "--clean".to_string(),
            "workload=x.yaml".to_string(),
        ]));
        assert!(!is_clean_mode(&[
            "all".to_string(),
            "workload=x.yaml".to_string(),
        ]));
    }

    #[test]
    fn glob_star_matches() {
        assert!(glob_matches("recall*", "recall_at_k10"));
        assert!(glob_matches("*at_k10", "recall_at_k10"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*at*", "recall_at_k10"));
        assert!(!glob_matches("plot*", "recall"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_matches("plot?", "plot1"));
        assert!(!glob_matches("plot?", "plot12"));
    }

    #[test]
    fn glob_exact() {
        assert!(glob_matches("recall", "recall"));
        assert!(!glob_matches("recall", "recall_at_k10"));
    }

    // ── Figure-selector parser ──────────────────────────────

    #[test]
    fn figure_selector_single_index() {
        assert_eq!(parse_figure_selector("5"), Some(vec![5]));
        assert_eq!(parse_figure_selector(" 5 "), Some(vec![5]));
    }

    #[test]
    fn figure_selector_hyphen_range() {
        assert_eq!(parse_figure_selector("2-4"), Some(vec![2, 3, 4]));
        assert_eq!(parse_figure_selector("1-1"), Some(vec![1]));
    }

    #[test]
    fn figure_selector_rust_range_inclusive() {
        // Both `..` and `..=` resolve to inclusive in this
        // CLI surface — humans typing `2..4` usually mean
        // "through 4," not "stop short of 4."
        assert_eq!(parse_figure_selector("2..4"), Some(vec![2, 3, 4]));
        assert_eq!(parse_figure_selector("2..=4"), Some(vec![2, 3, 4]));
    }

    #[test]
    fn figure_selector_comma_list() {
        assert_eq!(parse_figure_selector("2,3,4"), Some(vec![2, 3, 4]));
        assert_eq!(parse_figure_selector("1, 3 ,5"), Some(vec![1, 3, 5]));
    }

    #[test]
    fn figure_selector_mixed_list_with_ranges() {
        assert_eq!(parse_figure_selector("1,3-5,7"), Some(vec![1, 3, 4, 5, 7]),);
        assert_eq!(parse_figure_selector("1,3..5,7"), Some(vec![1, 3, 4, 5, 7]),);
    }

    #[test]
    fn figure_selector_preserves_user_order() {
        // No automatic sort/dedup — `5,3,1` renders 5 then 3
        // then 1.
        assert_eq!(parse_figure_selector("5,3,1"), Some(vec![5, 3, 1]));
    }

    #[test]
    fn figure_selector_rejects_non_numeric() {
        assert_eq!(parse_figure_selector("recall"), None);
        assert_eq!(parse_figure_selector("recall_at_k10"), None);
        assert_eq!(parse_figure_selector("2,abc"), None);
    }

    #[test]
    fn figure_selector_rejects_zero_and_negative() {
        // 0 is invalid (1-based indexing); `-5` looks like a
        // negative literal which is a missing-LHS hyphen
        // range and rejected.
        assert_eq!(parse_figure_selector("0"), None);
        assert_eq!(parse_figure_selector("-5"), None);
        assert_eq!(parse_figure_selector("0-5"), None);
    }

    #[test]
    fn figure_selector_rejects_reversed_ranges() {
        // `5-2` is rejected rather than treated as reversed
        // iteration — disambiguate via comma list (`5,4,3,2`).
        assert_eq!(parse_figure_selector("5-2"), None);
        assert_eq!(parse_figure_selector("4..2"), None);
    }

    #[test]
    fn figure_selector_rejects_empty() {
        assert_eq!(parse_figure_selector(""), None);
        assert_eq!(parse_figure_selector("  "), None);
        assert_eq!(parse_figure_selector(",,"), None);
    }
}
