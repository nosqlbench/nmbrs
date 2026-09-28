// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Backup-rotation transaction for workload edits.
//!
//! Per SRD-64 §6.5: every workload mutation rotates two
//! sibling files alongside the workload:
//!
//! - `<workload>.bak` — the immediate-previous content,
//!   written *before* the new content lands on disk.
//! - `<workload>.bak.prev` — one step further back (the
//!   pre-previous content). Lets the user recover from
//!   "the last edit was right but the one before was the
//!   one I wanted" without reaching for git.
//!
//! Two-deep is the policy floor; deeper history belongs in
//! version control.
//!
//! ## Rotation order
//!
//! Three steps per edit, all `fs::rename` for atomicity:
//!
//! 1. Move `<workload>.bak` → `<workload>.bak.prev`
//!    (overwriting any existing `.bak.prev`).
//! 2. Copy `<workload>` → `<workload>.bak`. Copy, not
//!    rename, because the workload itself has to stay in
//!    place until step 3 (otherwise readers see a missing
//!    file mid-edit). On filesystems that support reflinks
//!    (btrfs, xfs), the copy is essentially free.
//! 3. Write the new content to `<workload>` via a temp
//!    file + atomic rename so the workload itself is never
//!    half-written.
//!
//! Failure during step 1 leaves the prior backup pair
//! untouched. Failure during step 2 leaves a mismatched
//! pair (`bak.prev` is the old `bak`, `bak` is missing) —
//! the rollback path restores `bak.prev` → `bak` to keep
//! the pair consistent. Failure during step 3 leaves the
//! workload at its pre-edit state with a fresh `bak` that
//! matches it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Sibling-file paths for one workload.
#[derive(Debug, Clone)]
pub struct BackupPaths {
    /// The workload file itself.
    pub workload: PathBuf,
    /// `<workload>.bak` — most-recent prior content.
    pub bak: PathBuf,
    /// `<workload>.bak.prev` — one step further back.
    pub bak_prev: PathBuf,
    /// Tempfile used for the atomic write of the new
    /// content; renamed over `workload` once the buffer is
    /// flushed.
    pub temp: PathBuf,
}

impl BackupPaths {
    /// Derive the sibling-file paths for `workload_path`.
    /// Convention: `<file>.bak`, `<file>.bak.prev`, and
    /// `<file>.tmp` all live in the same directory as the
    /// workload, so the renames stay within one filesystem
    /// (atomic).
    pub fn for_workload(workload_path: &Path) -> Self {
        let workload = workload_path.to_path_buf();
        let bak = path_with_suffix(&workload, ".bak");
        let bak_prev = path_with_suffix(&workload, ".bak.prev");
        let temp = path_with_suffix(&workload, ".tmp");
        Self {
            workload,
            bak,
            bak_prev,
            temp,
        }
    }
}

fn path_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Run the backup rotation: rotate `.bak` → `.bak.prev`,
/// copy `workload` → `.bak`. Caller subsequently writes the
/// new content to `temp` and atomically renames it over
/// `workload`. See [`commit_temp`].
///
/// Returns the [`BackupPaths`] so the caller doesn't have
/// to re-derive them.
pub fn rotate(workload_path: &Path) -> io::Result<BackupPaths> {
    let paths = BackupPaths::for_workload(workload_path);
    if !paths.workload.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "workload '{}' does not exist; cannot create backup",
                paths.workload.display()
            ),
        ));
    }

    // Step 1: bak → bak.prev. Either may be absent on the
    // first edit; absence is fine, we just skip.
    if paths.bak.exists() {
        // Remove any stale bak.prev so the rename succeeds
        // on Windows (which fails rename when destination
        // exists; Unix overwrites silently).
        let _ = fs::remove_file(&paths.bak_prev);
        fs::rename(&paths.bak, &paths.bak_prev).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "backup rotate {} → {}: {e}",
                    paths.bak.display(),
                    paths.bak_prev.display()
                ),
            )
        })?;
    }

    // Step 2: workload → bak (copy, not rename — the
    // workload has to stay in place for callers reading it
    // mid-edit).
    fs::copy(&paths.workload, &paths.bak).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "backup copy {} → {}: {e}",
                paths.workload.display(),
                paths.bak.display()
            ),
        )
    })?;

    Ok(paths)
}

/// Atomically replace the workload with the contents of
/// `temp`. Caller has written the new content to `temp`;
/// this rename promotes it to the workload's path. After
/// this returns, `<workload>.bak` holds the pre-edit
/// content and the workload itself holds the post-edit
/// content.
pub fn commit_temp(paths: &BackupPaths) -> io::Result<()> {
    fs::rename(&paths.temp, &paths.workload).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "commit {} → {}: {e}",
                paths.temp.display(),
                paths.workload.display()
            ),
        )
    })
}

/// Roll back a partially-applied rotation. Used when the
/// in-memory mutation step fails (or its post-write parse
/// fails). Restores the on-disk state to what it was
/// before [`rotate`] ran:
///
/// - The workload itself wasn't touched by `rotate` (we
///   copied, didn't move), so it's already correct.
/// - `<workload>.bak` was overwritten by the copy from the
///   workload — that's a no-op semantically (the bak now
///   matches the workload, same as it would after a clean
///   commit on the same content). To preserve the
///   "<workload>.bak holds the pre-edit content" invariant
///   strictly, we restore the previous `.bak` from
///   `.bak.prev` (which was the prior pre-edit content
///   before the rotate).
/// - `<workload>.bak.prev` is restored to whatever it was
///   before the rotate — but we don't have that; the
///   rotation overwrote it. Best effort: leave the current
///   `.bak.prev` as the recovery point.
///
/// In short: roll back the bak↔bak.prev swap so the
/// invariant "<workload>.bak == content prior to the most
/// recent successful edit" holds.
pub fn rollback(paths: &BackupPaths) -> io::Result<()> {
    // If the temp file exists from an aborted write, drop it.
    let _ = fs::remove_file(&paths.temp);

    // Reverse step 1 — bak.prev → bak — if a prev exists.
    // This restores the .bak to what it was before the
    // (failed) edit kicked off.
    if paths.bak_prev.exists() {
        let _ = fs::remove_file(&paths.bak);
        fs::rename(&paths.bak_prev, &paths.bak).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "backup rollback {} → {}: {e}",
                    paths.bak_prev.display(),
                    paths.bak.display()
                ),
            )
        })?;
    } else {
        // No prev — first edit. Drop the new bak so we
        // return to the no-history state.
        let _ = fs::remove_file(&paths.bak);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn fresh_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "nmbrs-edit-backup-{label}-{}-{n}",
            std::process::id(),
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn first_edit_creates_bak_only() {
        let dir = fresh_dir("first_edit");
        let workload = dir.join("w.yaml");
        fs::write(&workload, b"v1\n").unwrap();
        let paths = rotate(&workload).expect("rotate");
        assert!(paths.bak.exists(), ".bak should exist");
        assert!(
            !paths.bak_prev.exists(),
            ".bak.prev should not exist on first edit"
        );
        assert_eq!(fs::read(&paths.bak).unwrap(), b"v1\n");
    }

    #[test]
    fn second_edit_promotes_bak_to_bak_prev() {
        let dir = fresh_dir("second_edit");
        let workload = dir.join("w.yaml");
        fs::write(&workload, b"v1\n").unwrap();
        let paths = rotate(&workload).expect("rotate v1");
        // Simulate a successful first edit: workload now
        // holds v2, .bak holds v1.
        fs::write(&workload, b"v2\n").unwrap();
        // Second edit: rotate again with workload=v2.
        let paths2 = rotate(&workload).expect("rotate v2");
        assert!(paths2.bak.exists());
        assert!(paths2.bak_prev.exists());
        assert_eq!(
            fs::read(&paths2.bak).unwrap(),
            b"v2\n",
            ".bak should hold the just-prior content (v2)"
        );
        assert_eq!(
            fs::read(&paths2.bak_prev).unwrap(),
            b"v1\n",
            ".bak.prev should hold the one-before-prior content (v1)"
        );
        // Original BackupPaths still consistent.
        let _ = paths;
    }

    #[test]
    fn third_edit_drops_oldest() {
        let dir = fresh_dir("third_edit");
        let workload = dir.join("w.yaml");
        fs::write(&workload, b"v1\n").unwrap();
        let _ = rotate(&workload).expect("rotate v1");
        fs::write(&workload, b"v2\n").unwrap();
        let _ = rotate(&workload).expect("rotate v2");
        fs::write(&workload, b"v3\n").unwrap();
        let paths = rotate(&workload).expect("rotate v3");
        // Two-deep: .bak=v3 (just-prior), .bak.prev=v2.
        // The original v1 is gone.
        assert_eq!(fs::read(&paths.bak).unwrap(), b"v3\n");
        assert_eq!(fs::read(&paths.bak_prev).unwrap(), b"v2\n");
    }

    #[test]
    fn commit_temp_renames_atomically() {
        let dir = fresh_dir("commit_temp");
        let workload = dir.join("w.yaml");
        fs::write(&workload, b"v1\n").unwrap();
        let paths = rotate(&workload).expect("rotate");
        fs::write(&paths.temp, b"v2 new content\n").unwrap();
        commit_temp(&paths).expect("commit");
        assert_eq!(fs::read(&workload).unwrap(), b"v2 new content\n");
        assert!(!paths.temp.exists(), "temp consumed by rename");
        assert_eq!(
            fs::read(&paths.bak).unwrap(),
            b"v1\n",
            ".bak still holds pre-edit content"
        );
    }

    #[test]
    fn rollback_after_first_edit_restores_no_history_state() {
        let dir = fresh_dir("rollback_first");
        let workload = dir.join("w.yaml");
        fs::write(&workload, b"v1\n").unwrap();
        let paths = rotate(&workload).expect("rotate");
        // Simulate the mutation step failing — caller never
        // wrote temp, never committed. Roll back.
        rollback(&paths).expect("rollback");
        assert!(
            !paths.bak.exists(),
            ".bak should be dropped on rollback of first edit"
        );
        assert!(!paths.bak_prev.exists());
        assert_eq!(
            fs::read(&workload).unwrap(),
            b"v1\n",
            "workload itself untouched"
        );
    }

    #[test]
    fn rollback_after_second_edit_restores_pre_rotate_state() {
        let dir = fresh_dir("rollback_second");
        let workload = dir.join("w.yaml");
        fs::write(&workload, b"v1\n").unwrap();
        let _ = rotate(&workload).expect("first rotate");
        fs::write(&workload, b"v2\n").unwrap();
        let paths2 = rotate(&workload).expect("second rotate");
        // Pre-rollback state: .bak=v2, .bak.prev=v1.
        // Roll back the second rotate. Expected post-state:
        // .bak=v1 (the original pre-history backup),
        // .bak.prev gone.
        rollback(&paths2).expect("rollback");
        assert_eq!(
            fs::read(&paths2.bak).unwrap(),
            b"v1\n",
            ".bak restored from .bak.prev"
        );
        assert!(!paths2.bak_prev.exists(), ".bak.prev consumed by rollback");
        assert_eq!(
            fs::read(&workload).unwrap(),
            b"v2\n",
            "workload still at v2 (rollback doesn't touch workload — that's the temp's job)"
        );
    }

    #[test]
    fn rotate_errors_when_workload_missing() {
        let dir = fresh_dir("missing_workload");
        let workload = dir.join("missing.yaml");
        let err = rotate(&workload).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }
}
