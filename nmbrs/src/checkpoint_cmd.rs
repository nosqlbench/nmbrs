// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs checkpoint` — operator tooling over the SRD-44a
//! `checkpoint.jsonl` event log.
//!
//! Two read-only subcommands:
//!
//! - `nmbrs checkpoint show <session>` — pretty-print the
//!   per-line event stream from `<session>/checkpoint.jsonl`.
//!   Bad lines log a warning to stderr and are skipped (the
//!   reader keeps going, matching the storage layer's
//!   forward-compat policy).
//! - `nmbrs checkpoint fold <session>` — emit the folded
//!   `Checkpoint` document — the equivalent of the legacy
//!   `checkpoint.json` shape — as pretty-printed JSON for
//!   diff / inspection.
//!
//! Session resolution mirrors `nmbrs-runtime::runner::run`:
//! a session arg can be a file (used directly), a directory
//! (we append `checkpoint.jsonl`), or a bare session id
//! (resolved as `logs/<id>/checkpoint.jsonl`).

use std::path::PathBuf;

use nmbrs_runtime::checkpoint::storage::{self, OpCounts};
use nmbrs_runtime::checkpoint::{CheckpointData, PathSegment, PhaseIdentity};

/// Resolve a session argument to an on-disk `checkpoint.jsonl`
/// path, mirroring the runner's resume-target resolution
/// (see `nmbrs-runtime::runner::run`):
///
/// 1. existing file → used verbatim
/// 2. existing directory → joined with `checkpoint.jsonl`
/// 3. otherwise → treated as a session id under `logs/<id>/`
///
/// Returns the candidate path even when it doesn't exist so
/// the caller can produce a clear "not found" diagnostic
/// rather than a generic ENOENT.
fn resolve_checkpoint_path(arg: &str) -> PathBuf {
    let p = PathBuf::from(arg);
    if p.is_file() {
        p
    } else if p.is_dir() {
        p.join("checkpoint.jsonl")
    } else {
        nmbrs_runtime::session::session_dir_named(arg).join("checkpoint.jsonl")
    }
}

/// `nmbrs checkpoint show <session>` — pretty-print the JSONL
/// event stream one event per line.
pub fn show_command(session: &str) -> Result<(), String> {
    let path = resolve_checkpoint_path(session);
    let iter = storage::iter_events(&path)?;
    let Some(iter) = iter else {
        return Err(format!("checkpoint log not found at '{}'", path.display(),));
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    use std::io::Write as _;
    for ev in iter {
        match ev {
            Ok(e) => {
                let line = render_event(&e);
                let _ = writeln!(out, "{line}");
            }
            Err(e) => {
                // Don't abort — the storage-level fold reader
                // already treats mid-stream parse failures as
                // recoverable, so the operator-facing show
                // command should too. SRD-44a §"Reader
                // behaviour".
                eprintln!("warning: {e}");
            }
        }
    }
    Ok(())
}

/// `nmbrs checkpoint fold <session>` — emit the folded
/// `Checkpoint` document as pretty JSON. Equivalent of the
/// legacy `checkpoint.json` shape.
pub fn fold_command(session: &str) -> Result<(), String> {
    let path = resolve_checkpoint_path(session);
    match storage::read(&path)? {
        None => {
            eprintln!(
                "nmbrs checkpoint fold: no events at '{}' (empty or missing log)",
                path.display(),
            );
            Ok(())
        }
        Some(doc) => {
            let json = serde_json::to_string_pretty(&doc)
                .map_err(|e| format!("serialize checkpoint to JSON: {e}"))?;
            println!("{json}");
            Ok(())
        }
    }
}

// ── event rendering ───────────────────────────────────────────

/// Format one event as a human-readable status line.
/// Format: `<timestamp>  <event_type>  <details>`. The
/// timestamp + type columns are left-aligned at fixed widths
/// for scanability; details are variant-specific.
pub fn render_event(e: &CheckpointData) -> String {
    // Pad event-type column to 18 chars so paths line up.
    fn row(at: &str, kind: &str, body: &str) -> String {
        if body.is_empty() {
            format!("{at:<20}  {kind:<18}")
        } else {
            format!("{at:<20}  {kind:<18}  {body}")
        }
    }
    match e {
        CheckpointData::SessionStart {
            at,
            version,
            session,
            started_at,
            invocation,
        } => row(
            at,
            "session_start",
            &format!(
                "session={session} invocation={invocation} version={version} started_at={started_at}",
            ),
        ),
        CheckpointData::SessionEnd { at, outcome, error } => {
            let body = match error {
                Some(err) => format!("outcome={outcome} error={err}"),
                None => format!("outcome={outcome}"),
            };
            row(at, "session_end", &body)
        }
        CheckpointData::PhaseDeclared {
            at,
            identity,
            skip_eligible,
        } => row(
            at,
            "phase_declared",
            &format!(
                "{} (skip_eligible={skip_eligible})",
                format_identity(identity)
            ),
        ),
        CheckpointData::PhaseStarted { at, identity } => {
            row(at, "phase_started", &format_identity(identity))
        }
        CheckpointData::PhaseProgress {
            at,
            identity,
            op_counts,
            cursor_state,
        } => {
            let mut body = format!(
                "{} {}",
                format_identity(identity),
                format_op_counts(op_counts),
            );
            if cursor_state.is_some() {
                body.push_str(" cursor=present");
            }
            row(at, "phase_progress", &body)
        }
        CheckpointData::PhaseCompleted {
            at,
            identity,
            duration_secs,
            op_counts,
        } => row(
            at,
            "phase_completed",
            &format!(
                "{} duration={:.3}s {}",
                format_identity(identity),
                duration_secs,
                format_op_counts(op_counts),
            ),
        ),
        CheckpointData::PhaseFailed {
            at,
            identity,
            error,
            op_counts,
        } => {
            let counts = op_counts
                .as_ref()
                .map(|c| format!(" {}", format_op_counts(c)))
                .unwrap_or_default();
            row(
                at,
                "phase_failed",
                &format!("{} error={error}{counts}", format_identity(identity),),
            )
        }
        CheckpointData::PhaseHash {
            at,
            identity,
            hash_hex,
            params_consumed,
        } => {
            // 12-char hash prefix is enough for visual diffing
            // without flooding the line. SRD-107: show the
            // consumed-param NAMES (values are digests — the
            // names are what an operator diffs against a "why
            // did this re-run?" question).
            let prefix: String = hash_hex.chars().take(12).collect();
            let params_note = params_consumed
                .as_deref()
                .and_then(|j| {
                    serde_json::from_str::<std::collections::BTreeMap<String, String>>(j).ok()
                })
                .filter(|m| !m.is_empty())
                .map(|m| {
                    format!(
                        " params=[{}]",
                        m.keys().cloned().collect::<Vec<_>>().join(","),
                    )
                })
                .unwrap_or_default();
            row(
                at,
                "phase_hash",
                &format!("{} hash={prefix}…{params_note}", format_identity(identity)),
            )
        }
        CheckpointData::ScopeEnter {
            at, kind, coords, ..
        } => row(
            at,
            "scope_enter",
            &format!("kind={kind} coords={}", format_coords_map(coords)),
        ),
        CheckpointData::ScopeExit {
            at,
            kind,
            coords,
            outcome,
            ..
        } => row(
            at,
            "scope_exit",
            &format!(
                "kind={kind} outcome={outcome} coords={}",
                format_coords_map(coords),
            ),
        ),
    }
}

/// Render a phase identity as `path…/leaf coords` (or just
/// the leaf path when coords is empty). Keeps the show-line
/// short while still distinguishing iterations.
fn format_identity(id: &PhaseIdentity) -> String {
    let path = format_yaml_path(&id.yaml_path);
    if id.coords.is_empty() {
        path
    } else {
        format!("{path} {}", id.coords)
    }
}

/// Format a yaml-path as a slash-joined display string.
fn format_yaml_path(path: &[PathSegment]) -> String {
    if path.is_empty() {
        return "<root>".to_string();
    }
    path.iter()
        .map(format_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn format_segment(seg: &PathSegment) -> String {
    match seg {
        PathSegment::Scenario(n) => format!("scenario:{n}"),
        PathSegment::ScenarioInclude(n) => format!("include:{n}"),
        PathSegment::ForEach { var } => format!("for_each({var})"),
        PathSegment::ForCombinations { vars } => format!("for_comb({})", vars.join(",")),
        PathSegment::DoWhile { counter } => match counter {
            Some(c) => format!("do_while({c})"),
            None => "do_while".to_string(),
        },
        PathSegment::DoUntil { counter } => match counter {
            Some(c) => format!("do_until({c})"),
            None => "do_until".to_string(),
        },
        PathSegment::Phase(n) => n.clone(),
    }
}

fn format_op_counts(c: &OpCounts) -> String {
    format!(
        "started={} finished={} errors={}",
        c.started, c.finished, c.errors
    )
}

fn format_coords_map(m: &std::collections::BTreeMap<String, serde_json::Value>) -> String {
    if m.is_empty() {
        return "()".to_string();
    }
    let parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("({})", parts.join(","))
}

// ── cli_spec entry ────────────────────────────────────────────

/// `nmbrs checkpoint` — wraps the `show` and `fold` subcommands
/// behind a parent dispatcher. The parent prints usage if no
/// subcommand is given.
pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{
        Category, Command, Handler, Level, ParsedCommand, Positional, PositionalKind,
    };

    fn handle_bare(_p: ParsedCommand) -> Result<(), String> {
        print_checkpoint_usage();
        Ok(())
    }

    fn handle_show(p: ParsedCommand) -> Result<(), String> {
        let session = p
            .positional(0)
            .ok_or_else(|| "checkpoint show: missing <session> argument".to_string())?;
        show_command(session)
    }

    fn handle_fold(p: ParsedCommand) -> Result<(), String> {
        let session = p
            .positional(0)
            .ok_or_else(|| "checkpoint fold: missing <session> argument".to_string())?;
        fold_command(session)
    }

    Command {
        name: "checkpoint",
        help: "Inspect a session's SRD-44a checkpoint event log.",
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        handler: Some(Handler::Sync(handle_bare)),
        raw_args: false,
        completion_override: None,
        subcommands: vec![
            Command {
                name: "show",
                help: "Pretty-print the checkpoint event stream.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: Vec::new(),
                kv_params: &[],
                dynamic_options: None,
                positionals: vec![Positional {
                    name: "session",
                    help: "Session id (under logs/), session dir, or path to checkpoint.jsonl.",
                    value: crate::completion::SESSION_NAME_VALUE,
                    kind: PositionalKind::One,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_show)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "fold",
                help: "Emit the folded checkpoint document as JSON.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: Vec::new(),
                kv_params: &[],
                dynamic_options: None,
                positionals: vec![Positional {
                    name: "session",
                    help: "Session id (under logs/), session dir, or path to checkpoint.jsonl.",
                    value: crate::completion::SESSION_NAME_VALUE,
                    kind: PositionalKind::One,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_fold)),
                raw_args: false,
                completion_override: None,
            },
        ],
    }
}

fn print_checkpoint_usage() {
    eprintln!("Usage: nmbrs checkpoint <subcommand> <session>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  show <session>   Pretty-print the checkpoint event stream.");
    eprintln!("  fold <session>   Emit the folded checkpoint document as JSON.");
    eprintln!();
    eprintln!("<session> may be a session id under logs/, an absolute session");
    eprintln!("directory, or a direct path to a checkpoint.jsonl file.");
}

// ── tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_runtime::checkpoint::CheckpointWriter;
    use nmbrs_runtime::checkpoint::{PathSegment, PhaseIdentity};
    use std::path::Path;

    fn ident(name: &str) -> PhaseIdentity {
        PhaseIdentity {
            yaml_path: vec![
                PathSegment::Scenario("test".into()),
                PathSegment::Phase(name.into()),
            ],
            coords: String::new(),
            phase_hash: None,
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("nmbrs-checkpoint-cmd-{}", crate::scratch_suffix()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_sample_log(path: &Path) {
        let w = CheckpointWriter::new(
            path.to_path_buf(),
            "test_run".into(),
            "2026-05-07T12:00:00Z".into(),
            1,
        );
        let id1 = ident("schema");
        let id2 = ident("rampup");
        w.declare_phase(id1.clone(), true);
        w.declare_phase(id2.clone(), false);
        w.phase_started(&id1);
        w.phase_completed(&id1, 1.5);
        w.phase_started(&id2);
        w.update_op_counts(
            &id2,
            OpCounts {
                started: 100,
                finished: 99,
                errors: 1,
            },
        );
        w.flush().expect("flush");
    }

    #[test]
    fn show_renders_event_stream_lines() {
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        write_sample_log(&path);

        // Render every event the sample log contains and check
        // the human-friendly shape per event type.
        let events: Vec<CheckpointData> = storage::iter_events(&path)
            .expect("open log")
            .expect("present")
            .collect::<Result<Vec<_>, _>>()
            .expect("parse all");

        let lines: Vec<String> = events.iter().map(render_event).collect();

        // session_start opens the stream.
        assert!(
            lines[0].contains("session_start"),
            "first line: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("session=test_run"),
            "first line: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("invocation=1"),
            "first line: {}",
            lines[0]
        );

        // Each phase_declared mentions the phase name + skip flag.
        let declared: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("phase_declared"))
            .collect();
        assert_eq!(declared.len(), 2, "two declarations expected");
        assert!(
            declared
                .iter()
                .any(|l| l.contains("schema") && l.contains("skip_eligible=true"))
        );
        assert!(
            declared
                .iter()
                .any(|l| l.contains("rampup") && l.contains("skip_eligible=false"))
        );

        // phase_completed line carries duration + counts.
        let completed = lines
            .iter()
            .find(|l| l.contains("phase_completed"))
            .expect("expected a phase_completed");
        assert!(completed.contains("schema"), "completed: {completed}");
        assert!(
            completed.contains("duration=1.500s"),
            "completed: {completed}"
        );
        assert!(completed.contains("started="), "completed: {completed}");

        // The leading column is the timestamp; row() pads the
        // event-type column so phase_started lines all start at
        // the same offset.
        for l in &lines {
            assert!(l.starts_with("2026-"), "expected ISO ts prefix: {l}");
        }
    }

    #[test]
    fn show_skips_bad_lines_without_aborting() {
        // A malformed mid-stream line must not stop the iterator
        // at the storage level (it surfaces as Err) — show_command
        // logs a warning and keeps going. We exercise the iterator
        // contract directly because show_command writes to stdout.
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        write_sample_log(&path);
        // Append a malformed line followed by a valid blank trailer.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"this is not valid json\n").unwrap();
        drop(f);

        // The iterator yields Err for the bad line but the next()
        // call after still returns events from the prefix (here,
        // EOF since we appended at the tail). Confirm at least the
        // good prefix's events survive — the show command's loop
        // does the same Err→warn→continue dance.
        let mut good = 0usize;
        let mut bad = 0usize;
        for r in storage::iter_events(&path).unwrap().unwrap() {
            match r {
                Ok(_) => good += 1,
                Err(_) => bad += 1,
            }
        }
        assert!(good >= 5, "expected ≥5 good events (got {good})");
        assert_eq!(bad, 1, "expected exactly one parse error");
    }

    #[test]
    fn fold_emits_pretty_json_for_present_log() {
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        write_sample_log(&path);

        let doc = storage::read(&path).expect("read").expect("present");
        let json = serde_json::to_string_pretty(&doc).expect("serialize");

        // Pretty form has indented "phases" array and the
        // session metadata at the top level.
        assert!(json.contains("\"session\": \"test_run\""), "json: {json}");
        assert!(json.contains("\"invocation\": 1"), "json: {json}");
        assert!(json.contains("\"phases\":"), "json: {json}");
        assert!(json.contains("\"status\": \"completed\""), "json: {json}");
        assert!(json.contains("\"status\": \"running\""), "json: {json}");
        // Pretty-printer indents — check that the JSON spans
        // multiple lines so we know we're emitting the pretty
        // form, not the compact one.
        assert!(json.lines().count() > 5, "expected multi-line pretty JSON");
    }

    #[test]
    fn fold_returns_ok_on_missing_log() {
        let dir = tempdir();
        let path = dir.join("absent.jsonl");
        // Direct check of the underlying primitive — the command
        // wrapper just prints a stderr line in the None case.
        let r = storage::read(&path).expect("missing-file is Ok(None)");
        assert!(r.is_none());
    }

    #[test]
    fn resolve_path_handles_file_dir_and_id() {
        let dir = tempdir();
        let nested = dir.join("checkpoint.jsonl");
        std::fs::write(&nested, b"").unwrap();

        // 1) Direct file path.
        assert_eq!(resolve_checkpoint_path(nested.to_str().unwrap()), nested);
        // 2) Directory path → joins checkpoint.jsonl.
        assert_eq!(
            resolve_checkpoint_path(dir.to_str().unwrap()),
            dir.join("checkpoint.jsonl")
        );
        // 3) Bare id → <sessions-root>/<id>/checkpoint.jsonl
        //    (SRD-77 rename — `logs/` was the pre-SRD-77
        //    name; under cargo workspace the helper resolves
        //    to `<tmp>/nmbrs-sessions/` so we assert on the
        //    trailing structure instead of the prefix).
        let id_path = resolve_checkpoint_path("nonexistent_session_id");
        assert!(
            id_path.ends_with("nonexistent_session_id/checkpoint.jsonl"),
            "expected <sessions-root>/<id>/checkpoint.jsonl, got {id_path:?}"
        );
    }

    #[test]
    fn render_event_session_end_includes_outcome() {
        let e = CheckpointData::SessionEnd {
            at: "2026-05-07T12:00:05Z".into(),
            outcome: "completed".into(),
            error: None,
        };
        let line = render_event(&e);
        assert!(line.contains("session_end"));
        assert!(line.contains("outcome=completed"));
    }

    #[test]
    fn render_event_phase_failed_includes_error() {
        let e = CheckpointData::PhaseFailed {
            at: "2026-05-07T12:00:10Z".into(),
            identity: ident("rampup"),
            error: "boom".into(),
            op_counts: Some(OpCounts {
                started: 5,
                finished: 2,
                errors: 3,
            }),
        };
        let line = render_event(&e);
        assert!(line.contains("phase_failed"));
        assert!(line.contains("rampup"));
        assert!(line.contains("error=boom"));
        assert!(line.contains("errors=3"));
    }
}
