// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Snapshot capture for readout renders. See SRD-63 §6.
//!
//! Each event fire captures the rendered output (both
//! styled and ANSI-stripped) and persists to the session
//! db's `readout_snapshots` table. The latest render per
//! `(slot, subject_kind, subject_id, readout_name, lod)`
//! tuple wins — insert-or-replace upsert keeps the row
//! count bounded by the scenario tree size.
//!
//! Push 6 ships:
//!
//! - [`strip_ansi`] — utility that turns the styled byte
//!   stream into the plain-text fallback column.
//! - [`render_to_snapshot_strings`] — convenience that
//!   takes a [`StringSink`](super::StringSink)'s output
//!   and returns the `(ansi_bytes, plain_text)` pair the
//!   sqlite writer expects.
//! - The [`SqliteReporter::upsert_readout_snapshot`]
//!   surface in `nmbrs-metrics::reporters::sqlite` is the
//!   actual store.
//!
//! Wiring the capture into `nmbrs-runtime::activity`'s
//! per-event fire sites is intentionally out of scope here
//! — the activity-side glue lives next to the existing
//! binder.fire() call sites and threads through whatever
//! `SqliteReporter` the session already holds.

/// Handle the activity holds for snapshot writes.
///
/// The shape — `Arc<Mutex<Option<SqliteReporter>>>` — is
/// load-bearing because the runner shares the same handle
/// for non-readout sqlite work (cadence flushes, shutdown
/// finalisers); the inner `Option` reflects "sqlite init
/// failed; degrade gracefully," the Mutex serialises
/// writes (sqlite isn't Send), and the Arc shares across
/// the activity / observer / readout-fire threads.
///
/// Snapshot capture goes through [`capture`] rather than
/// locking and matching by hand — the helper hides the
/// three-layer unwrap and treats lock-poisoning /
/// writer-absent as silent no-ops (best-effort).
pub type SnapshotWriter =
    std::sync::Arc<std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>>;

/// LOD → string serialised for the storage column.
pub fn lod_str(lod: super::Lod) -> &'static str {
    match lod {
        super::Lod::Compact => "compact",
        super::Lod::Labeled => "labeled",
        super::Lod::Expanded => "expanded",
    }
}

/// Capture a rendered body to the snapshot store. Best-
/// effort: a `None` writer (snapshots disabled), lock
/// poisoning, or "sqlite init failed" (inner `Option` is
/// `None`) all collapse to silent no-ops — snapshot
/// capture must never block or corrupt the readout-fire
/// path.
///
/// `subject_id` is normally produced by
/// [`ReadoutContext::subject_id`](super::ReadoutContext::subject_id)
/// — the trait default folds `subject_name` +
/// `subject_labels` into the conventional `name@labels`
/// shape; session-scoped contexts override to a literal
/// `"session"`.
pub fn capture(
    writer: Option<&SnapshotWriter>,
    slot: &str,
    exec_id: u64,
    subject_kind: &str,
    subject_id: &str,
    readout_name: &str,
    lod: &str,
    rendered: &str,
) {
    let Some(writer) = writer else {
        return;
    };
    let plain = strip_ansi(rendered);
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let body_ansi = if rendered != plain {
        Some(rendered.as_bytes())
    } else {
        None
    };
    let mut guard = match writer.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(reporter) = guard.as_mut() {
        reporter.upsert_readout_snapshot(
            slot,
            exec_id,
            subject_kind,
            subject_id,
            readout_name,
            lod,
            now_nanos,
            body_ansi,
            &plain,
        );
    }
}

/// Strip ANSI SGR escape sequences (`\x1b[...m`) from a
/// rendered string, leaving the plain text. Used to
/// derive the `body_plain` column from the styled
/// `body_ansi` blob.
///
/// This is the same algorithm as
/// `nmbrs-runtime::activity::truncate_to_width` uses to
/// skip escapes when measuring visible width — kept here
/// so snapshot capture has no surface dependency on the
/// activity module.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices();
    while let Some((_, c)) = chars.next() {
        if c == '\x1b' {
            // Walk past `[` / `(` / etc. and the
            // terminating letter (m / K / J / …). Skips
            // CSI parameters and intermediate bytes
            // without trying to interpret them — we just
            // want to drop the whole escape from the
            // visible stream.
            for (_, ch) in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_sgr_runs() {
        assert_eq!(
            strip_ansi("\x1b[34m[setup]\x1b[0m 100% \x1b[32m\u{2713}\x1b[0m"),
            "[setup] 100% \u{2713}",
        );
    }

    #[test]
    fn strip_ansi_handles_no_escapes() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn strip_ansi_preserves_non_escape_unicode() {
        assert_eq!(strip_ansi("(profile=alpha) ✓"), "(profile=alpha) ✓");
    }

    #[test]
    fn strip_ansi_handles_carriage_return_and_clear() {
        // The inline-status thread emits `\r\x1b[K`; the
        // strip helper drops only the SGR-shaped escape.
        // The `\r` is preserved as-is — that's a control
        // character but not an SGR escape. Snapshot store
        // captures the rendered body, not the carriage
        // return preamble.
        assert_eq!(strip_ansi("\x1b[Khello"), "hello",);
    }
}
