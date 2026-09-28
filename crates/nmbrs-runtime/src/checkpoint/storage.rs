// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint storage — fold-state types + JSONL event-log
//! reader.
//!
//! See SRD-44a §"Reader behaviour" for the fold algorithm.
//! `Checkpoint` is the in-memory representation produced by
//! folding the on-disk `checkpoint.jsonl` event stream; each
//! per-phase `PhaseEntry` records the entry's identity, status,
//! and any cursor state for in-flight Tier 2 resume.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::events::{CheckpointData, hex_to_hash};
use super::identity::PhaseIdentity;

/// In-memory checkpoint state — the fold of an append-only
/// JSONL event stream at `logs/<session>/checkpoint.jsonl`.
/// One per session. Per SRD-44a, the on-disk format is the
/// event log; this struct is what consumers (resume planner,
/// summary report) see after reading and folding.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    /// File-format version. `1` until we ship a `2`. Each
    /// resume invocation refuses to read a checkpoint whose
    /// version it doesn't recognise — fail fast over silent
    /// schema drift.
    pub version: u32,
    /// Session identifier — same string used in
    /// `logs/<session>/`. The resume CLI's auto-detect picks
    /// the most-recent session; explicit `--resume <id>`
    /// names this directly.
    pub session: String,
    /// RFC 3339 timestamp of session start — first
    /// invocation's `nmbrs run` start.
    pub started_at: String,
    /// RFC 3339 timestamp of this flush — updated on every
    /// successful write.
    pub checkpoint_at: String,
    /// 1-based invocation counter. First `nmbrs run` is `1`;
    /// each `--resume` increments by 1. Used in `session.log`
    /// separator lines (`--- RESUMED <ts> [#N] ---`) and in
    /// post-run summary diagnostics if needed.
    pub invocation: u32,
    /// One entry per pre-mapped phase. Order matches the
    /// scenario tree's DFS — same order the post-run summary
    /// uses, same order the resume planner walks.
    pub phases: Vec<PhaseEntry>,
}

/// Per-phase entry in the checkpoint document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhaseEntry {
    /// Phase identity tuple. The resume planner uses
    /// `(yaml_path, coords)` as the structural match key and
    /// (when present) the `phase_hash` as the sufficiency
    /// check. See SRD-44 §"Phase identity".
    #[serde(flatten)]
    pub identity: PhaseIdentity,
    /// Whether this phase is *eligible to skip* on resume
    /// per its `checkpoint:` declaration. `true` means the
    /// resume planner may classify the phase as Skip when
    /// the saved status is Completed and identity matches.
    /// `false` (operator declared `checkpoint: none` or no
    /// declaration at all) means the phase always re-runs,
    /// regardless of saved status.
    pub skip_eligible: bool,
    /// SRD-107 — the consumed-params map as canonical JSON
    /// (`{"name":"<value sha256 hex>",…}`); the per-param leg of
    /// resume skip validity. `None` on entries written before
    /// the field existed (their base hash never matches current
    /// formulas, so they re-run regardless).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_consumed: Option<String>,
    /// Lifecycle status at the time of the most recent flush.
    pub status: PhaseStatus,
    /// Wall-clock duration of the *successful* execution.
    /// Set when status transitions to Completed; preserved on
    /// subsequent flushes. None for in-flight (`Running`) or
    /// terminally-failed phases.
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// Op counts from the live activity. Captured on every
    /// flush; useful for ETA and post-run summary in resumed
    /// sessions.
    #[serde(default)]
    pub op_counts: Option<OpCounts>,
    /// Tier 2 only. Opaque cursor-state snapshot from the
    /// active source factory, captured on each flush while
    /// the phase is `Running`. Resume planner restores this
    /// to a freshly-constructed cursor source so the phase
    /// continues from where it left off.
    #[serde(default)]
    pub cursor_state: Option<serde_json::Value>,
    /// Per-phase error message recorded when the phase
    /// transitioned to `Failed` — preserved across flushes
    /// so resume diagnostics can reference the original
    /// failure mode.
    #[serde(default)]
    pub error: Option<String>,
}

/// Lifecycle status as recorded in the checkpoint file.
/// Mirrors `crate::scene_tree::PhaseStatus` semantically; kept
/// as a separate type so the on-disk vocabulary doesn't
/// coupled-evolve with scene-tree internal status (e.g. if we
/// ever add a transient state for a runtime invariant that
/// has no on-disk meaning, the storage type stays clean).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl From<crate::scene_tree::PhaseStatus> for PhaseStatus {
    fn from(s: crate::scene_tree::PhaseStatus) -> Self {
        match s {
            crate::scene_tree::PhaseStatus::Pending => Self::Pending,
            crate::scene_tree::PhaseStatus::Running => Self::Running,
            crate::scene_tree::PhaseStatus::Completed => Self::Completed,
            crate::scene_tree::PhaseStatus::Failed(_) => Self::Failed,
        }
    }
}

/// Op-execution counts captured on every flush.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpCounts {
    pub started: u64,
    pub finished: u64,
    pub errors: u64,
}

/// Format the current wall-clock time as an RFC 3339 UTC
/// timestamp, e.g. `2026-01-01T00:00:00Z`. Used by the writer
/// for `started_at` / `checkpoint_at` and by the runner when
/// stamping a fresh session. Local implementation to avoid
/// dragging chrono in for one call site.
pub fn now_rfc3339() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Stream events from the JSONL log at `path`. Returns an
/// iterator over `CheckpointData` records; malformed lines
/// surface as `Err` items so the caller decides whether to
/// stop or continue. Truncated-tail recovery is handled by
/// [`read`]'s fold; this function is the lower-level building
/// block for diagnostics tools that want raw event streams.
pub fn iter_events(path: &Path) -> Result<Option<EventIter>, String> {
    match std::fs::File::open(path) {
        Ok(f) => {
            let reader = BufReader::new(f);
            Ok(Some(EventIter {
                lines: reader.lines(),
                path: path.to_path_buf(),
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read checkpoint log {}: {e}", path.display())),
    }
}

/// Iterator over [`CheckpointData`] records from a JSONL log.
pub struct EventIter {
    lines: std::io::Lines<BufReader<std::fs::File>>,
    path: std::path::PathBuf,
}

impl Iterator for EventIter {
    type Item = Result<CheckpointData, String>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let line = match self.lines.next()? {
                Ok(l) => l,
                Err(e) => return Some(Err(format!("read line from {}: {e}", self.path.display()))),
            };
            if line.trim().is_empty() {
                continue;
            }
            return Some(
                serde_json::from_str(&line)
                    .map_err(|e| format!("parse line in {}: {e}", self.path.display())),
            );
        }
    }
}

/// Read and fold every event in `path` into a [`Checkpoint`].
/// Returns `Ok(None)` when the file doesn't exist (fresh
/// session); returns `Err` only on hard parse failures
/// mid-stream. A truncated last line is recovered with a Warn
/// per SRD-44a §"Truncated-tail recovery".
pub fn read(path: &Path) -> Result<Option<Checkpoint>, String> {
    use std::collections::HashMap;

    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read checkpoint log {}: {e}", path.display())),
    };

    // Trim a partial trailing line (one that doesn't end in
    // `\n`) — this is the truncated-tail recovery. Anything
    // before the last `\n` is a complete record per the
    // append-mode write guarantee.
    let cutoff = raw
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    if cutoff < raw.len() {
        eprintln!(
            "warning: checkpoint {}: truncated tail (last {} bytes lacked newline), dropping",
            path.display(),
            raw.len() - cutoff,
        );
    }
    let body = &raw[..cutoff];
    let body_str = std::str::from_utf8(body)
        .map_err(|e| format!("checkpoint log {}: invalid UTF-8: {e}", path.display()))?;

    // First record MUST be a session_start per SRD-44a.
    let mut lines = body_str.lines().filter(|l| !l.trim().is_empty());
    let first_line = match lines.next() {
        Some(l) => l,
        None => return Ok(None), // empty log — treat as fresh
    };
    let first_event: CheckpointData = serde_json::from_str(first_line).map_err(|e| {
        format!(
            "checkpoint log {}: malformed first record: {e}",
            path.display()
        )
    })?;

    let mut doc = match first_event {
        CheckpointData::SessionStart {
            version,
            session,
            started_at,
            invocation,
            at,
            ..
        } => {
            if version != 1 {
                return Err(format!(
                    "checkpoint {}: unsupported version {version} (this build supports v1)",
                    path.display(),
                ));
            }
            Checkpoint {
                version,
                session,
                started_at,
                checkpoint_at: at,
                invocation,
                phases: Vec::new(),
            }
        }
        other => {
            return Err(format!(
                "checkpoint {}: first record must be session_start, got {:?}",
                path.display(),
                discriminator(&other),
            ));
        }
    };

    let mut index: HashMap<String, usize> = HashMap::new();

    for line in lines {
        let event: CheckpointData = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                // Per SRD-44a forward-compat: unknown event
                // types (which serde rejects since we use
                // tagged enum) are downgraded to a Debug log.
                // Same fate for malformed mid-file records —
                // they're a strong signal of corruption but
                // not load-bearing for fold correctness as
                // long as we keep going. Switching to Warn for
                // visibility.
                eprintln!(
                    "warning: checkpoint {}: ignoring unparseable line: {e}",
                    path.display(),
                );
                continue;
            }
        };
        apply_event(&mut doc, &mut index, event);
    }

    Ok(Some(doc))
}

fn discriminator(e: &CheckpointData) -> &'static str {
    match e {
        CheckpointData::SessionStart { .. } => "session_start",
        CheckpointData::SessionEnd { .. } => "session_end",
        CheckpointData::PhaseDeclared { .. } => "phase_declared",
        CheckpointData::PhaseStarted { .. } => "phase_started",
        CheckpointData::PhaseProgress { .. } => "phase_progress",
        CheckpointData::PhaseCompleted { .. } => "phase_completed",
        CheckpointData::PhaseFailed { .. } => "phase_failed",
        CheckpointData::PhaseHash { .. } => "phase_hash",
        CheckpointData::ScopeEnter { .. } => "scope_enter",
        CheckpointData::ScopeExit { .. } => "scope_exit",
    }
}

fn apply_event(
    doc: &mut Checkpoint,
    index: &mut std::collections::HashMap<String, usize>,
    event: CheckpointData,
) {
    match event {
        CheckpointData::SessionStart {
            invocation,
            at,
            started_at,
            session,
            ..
        } => {
            // Resume continuation: bump the invocation and
            // refresh the per-flush timestamps. The phase list
            // built so far stays as-is (fold semantics).
            doc.invocation = invocation;
            doc.checkpoint_at = at;
            doc.started_at = started_at;
            doc.session = session;
        }
        CheckpointData::SessionEnd { at, .. } => {
            doc.checkpoint_at = at;
        }
        CheckpointData::PhaseDeclared {
            at,
            identity,
            skip_eligible,
        } => {
            let key = super::writer::identity_key(&identity);
            if let std::collections::hash_map::Entry::Vacant(e) = index.entry(key) {
                doc.phases.push(PhaseEntry {
                    identity,
                    skip_eligible,
                    params_consumed: None,
                    status: PhaseStatus::Pending,
                    duration_secs: None,
                    op_counts: None,
                    cursor_state: None,
                    error: None,
                });
                e.insert(doc.phases.len() - 1);
            }
            doc.checkpoint_at = at;
        }
        CheckpointData::PhaseStarted { at, identity } => {
            if let Some(entry) = lookup_mut(doc, index, &identity) {
                entry.status = PhaseStatus::Running;
                entry.error = None;
            }
            doc.checkpoint_at = at;
        }
        CheckpointData::PhaseProgress {
            at,
            identity,
            op_counts,
            cursor_state,
        } => {
            if let Some(entry) = lookup_mut(doc, index, &identity) {
                entry.op_counts = Some(op_counts);
                if cursor_state.is_some() {
                    entry.cursor_state = cursor_state;
                }
            }
            doc.checkpoint_at = at;
        }
        CheckpointData::PhaseCompleted {
            at,
            identity,
            duration_secs,
            op_counts,
        } => {
            if let Some(entry) = lookup_mut(doc, index, &identity) {
                entry.status = PhaseStatus::Completed;
                entry.duration_secs = Some(duration_secs);
                entry.op_counts = Some(op_counts);
                entry.cursor_state = None;
                entry.error = None;
            }
            doc.checkpoint_at = at;
        }
        CheckpointData::PhaseFailed {
            at,
            identity,
            error,
            op_counts,
        } => {
            if let Some(entry) = lookup_mut(doc, index, &identity) {
                entry.status = PhaseStatus::Failed;
                entry.error = Some(error);
                if let Some(c) = op_counts {
                    entry.op_counts = Some(c);
                }
                entry.cursor_state = None;
            }
            doc.checkpoint_at = at;
        }
        CheckpointData::PhaseHash {
            at,
            identity,
            hash_hex,
            params_consumed,
        } => {
            if let Some(entry) = lookup_mut(doc, index, &identity)
                && let Some(h) = hex_to_hash(&hash_hex)
            {
                entry.identity.phase_hash = Some(h);
                entry.params_consumed = params_consumed;
            }
            doc.checkpoint_at = at;
        }
        CheckpointData::ScopeEnter { at, .. } | CheckpointData::ScopeExit { at, .. } => {
            // Push 3 territory — fold to nothing today; the
            // event lives on disk for forensic replay.
            doc.checkpoint_at = at;
        }
    }
}

fn lookup_mut<'a>(
    doc: &'a mut Checkpoint,
    index: &std::collections::HashMap<String, usize>,
    identity: &PhaseIdentity,
) -> Option<&'a mut PhaseEntry> {
    let key = super::writer::identity_key(identity);
    let idx = *index.get(&key)?;
    Some(&mut doc.phases[idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::CheckpointWriter;
    use crate::checkpoint::PathSegment;
    use crate::checkpoint::PhaseIdentity;

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

    #[test]
    fn read_missing_file_yields_none() {
        let dir = tempdir();
        let path = dir.join("nonexistent.jsonl");
        let result = read(&path).expect("read should not error on missing file");
        assert!(result.is_none());
    }

    #[test]
    fn read_empty_file_yields_none() {
        let dir = tempdir();
        let path = dir.join("empty.jsonl");
        std::fs::write(&path, "").expect("write");
        let result = read(&path).expect("read should not error on empty file");
        assert!(result.is_none(), "empty log = fresh session");
    }

    #[test]
    fn fold_full_lifecycle_matches_in_memory_snapshot() {
        // Write events via the writer, then read them back via
        // the fold algorithm; the two views must agree on every
        // field a resume planner observes.
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        let snap_in_memory = {
            let w = CheckpointWriter::new(
                path.clone(),
                "sess".into(),
                "2026-01-01T00:00:00Z".into(),
                1,
            );
            let id1 = ident("schema");
            let id2 = ident("rampup");
            w.declare_phase(id1.clone(), true);
            w.declare_phase(id2.clone(), true);
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
            w.snapshot()
        };
        let folded = read(&path).expect("read").expect("present");
        assert_eq!(folded.session, snap_in_memory.session);
        assert_eq!(folded.invocation, snap_in_memory.invocation);
        assert_eq!(folded.phases.len(), snap_in_memory.phases.len());
        for (i, phase) in folded.phases.iter().enumerate() {
            assert_eq!(
                phase.status, snap_in_memory.phases[i].status,
                "status mismatch on phase {i}"
            );
            assert_eq!(phase.skip_eligible, snap_in_memory.phases[i].skip_eligible);
            assert_eq!(phase.duration_secs, snap_in_memory.phases[i].duration_secs);
            assert_eq!(
                phase.op_counts.as_ref().map(|c| c.started),
                snap_in_memory.phases[i]
                    .op_counts
                    .as_ref()
                    .map(|c| c.started)
            );
        }
    }

    #[test]
    fn truncated_tail_is_recovered() {
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        {
            let w =
                CheckpointWriter::new(path.clone(), "s".into(), "2026-01-01T00:00:00Z".into(), 1);
            w.declare_phase(ident("p"), true);
            w.flush().expect("flush");
        }
        // Append a partial line (no trailing newline) — simulates
        // a crash mid-write.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"type\":\"phase_started\",\"at\":\"2026-")
            .unwrap();
        drop(f);

        let folded = read(&path).expect("read should recover").expect("present");
        // Truncated tail dropped, prior records intact.
        assert_eq!(folded.phases.len(), 1);
    }

    #[test]
    fn first_record_must_be_session_start() {
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        // A valid `phase_started` record shaped correctly but
        // appearing as the first line — the reader rejects.
        let body = r#"{"type":"phase_started","at":"x","identity":{"yaml_path":[],"coords":""}}"#;
        std::fs::write(&path, format!("{body}\n")).expect("write");
        let err = read(&path).expect_err("first-record check must error");
        assert!(
            err.contains("first record must be session_start"),
            "got: {err}"
        );
    }

    #[test]
    fn unsupported_version_in_session_start_errors() {
        let dir = tempdir();
        let path = dir.join("checkpoint.jsonl");
        let body = r#"{"type":"session_start","at":"t","version":99,"session":"x","started_at":"t","invocation":1}"#;
        std::fs::write(&path, format!("{body}\n")).expect("write");
        let err = read(&path).expect_err("expected version-mismatch error");
        assert!(err.contains("version 99"), "got: {err}");
    }

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nmbrs-checkpoint-test-{}", rand_suffix()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rand_suffix() -> String {
        crate::scratch_suffix()
    }
}
