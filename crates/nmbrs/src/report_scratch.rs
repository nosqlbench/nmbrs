// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Scratch lifecycle for `nmbrs report` (SRD-64 §5, §7).
//!
//! Single-component renders without `--add` land in
//! `<session>/scratch/`. They're session-local, disposable,
//! and out of `summary.md`'s assembly path so iterating on
//! a directive spec doesn't churn the assembled report.
//!
//! This module owns:
//!
//! - destination path resolution for one rendered item
//!   ([`scratch_paths`]),
//! - the timestamped name generator for `--name auto` and
//!   the no-name positional shorthand ([`timestamp_id`]),
//! - the `nmbrs report scratch list / clean / promote`
//!   subcommand dispatch ([`scratch_subcommand`]).
//!
//! Renderer integration is the caller's job. The text /
//! file paths are deterministic given the item name + kind +
//! session dir; the caller writes the rendered bytes to
//! the path returned by [`scratch_paths`].

use std::fs;
use std::path::{Path, PathBuf};

use nmbrs_workload::report::{Kind, ReportItem};

/// Output destinations for one scratch render.
#[derive(Debug, Clone)]
pub struct ScratchPaths {
    /// Markdown stub at `<session>/scratch/<name>.md`. For
    /// plots, the stub embeds `![](plot_<name>.png)`. For
    /// tables and text, the stub *is* the rendered output.
    pub md: PathBuf,
    /// PNG path for plot kinds: `<session>/plot_<name>.png`.
    /// Same naming scheme `auto_render_plots` uses for
    /// non-scratch renders, so the user can copy a scratch
    /// PNG into a published report without renaming.
    /// `None` for non-plot kinds.
    pub png: Option<PathBuf>,
}

/// Compute the on-disk paths for a scratch render of
/// `item` under `session_dir`. Creates the
/// `<session>/scratch/` directory if it doesn't exist;
/// other directories are not created (the caller's writer
/// uses the final paths' parents, which already exist).
pub fn scratch_paths(session_dir: &Path, item: &ReportItem) -> std::io::Result<ScratchPaths> {
    let scratch_dir = session_dir.join("scratch");
    fs::create_dir_all(&scratch_dir)?;
    let md = scratch_dir.join(format!("{}.md", item.name));
    let png = match item.kind {
        Kind::Plot => Some(session_dir.join(format!("plot_{}.png", item.name))),
        _ => None,
    };
    Ok(ScratchPaths { md, png })
}

/// Generate a timestamped scratch identifier:
/// `YYYYMMDD_HHmmss_<6-char hex from process id + nanos>`.
/// Used by `--name auto` and the no-positional-name path.
///
/// The 6-char hex tail is just an in-process disambiguator
/// so two scratch renders within the same second don't
/// collide. Determinism isn't required; cheap uniqueness
/// is.
pub fn timestamp_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    let pid = std::process::id();
    // YYYYMMDD_HHmmss — same shape `format_timestamp` uses
    // in `nmbrs_runtime::session`, kept inline so this
    // module doesn't pull in nmbrs-runtime for one helper.
    let (y, mo, d, h, mi, s) = ymd_hms(secs);
    let tail = ((pid as u64)
        .wrapping_mul(1_000_003)
        .wrapping_add(nanos as u64))
        & 0xFF_FFFF;
    format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}_{tail:06x}")
}

fn ymd_hms(unix_secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    // Civil-from-days algorithm (Howard Hinnant). Cheap and
    // exact for the range we care about; avoids pulling in
    // `chrono` or `time` for one timestamp.
    let days_since_epoch = unix_secs / 86_400;
    let rem = unix_secs % 86_400;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    let z = days_since_epoch as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as u64, m, d, h, mi, s)
}

/// Dispatcher for `nmbrs report scratch <subcommand>`.
/// Subcommands: `list`, `clean`, `promote`.
///
/// `args` is the slice after `nmbrs report scratch`; the
/// first token is the subcommand.
pub fn scratch_subcommand(session_dir: &Path, args: &[String]) {
    let scratch_dir = session_dir.join("scratch");
    match args.first().map(String::as_str) {
        None | Some("list") => list(&scratch_dir),
        Some("clean") => clean(&scratch_dir),
        Some("promote") => {
            // Phase D wires the actual promotion. For now
            // print a clear message so the surface is
            // visible.
            eprintln!(
                "nmbrs report scratch promote: pending Phase D — \
                 once the workload-edit + anchor-walk are wired, \
                 this will route to `nmbrs report <kind> <name> --add`. \
                 In the meantime, re-run the original render command \
                 with `--add` to promote."
            );
            std::process::exit(2);
        }
        Some(other) => {
            eprintln!("nmbrs report scratch: unknown subcommand '{other}'");
            eprintln!("usage: nmbrs report scratch [list|clean|promote]");
            std::process::exit(2);
        }
    }
}

fn list(scratch_dir: &Path) {
    if !scratch_dir.exists() {
        println!("(no scratch directory at {})", scratch_dir.display());
        return;
    }
    let mut entries: Vec<_> = match fs::read_dir(scratch_dir) {
        Ok(it) => it.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("nmbrs report scratch list: {e}");
            std::process::exit(2);
        }
    };
    entries.sort_by_key(|e| e.file_name());
    if entries.is_empty() {
        println!("(scratch directory empty: {})", scratch_dir.display());
        return;
    }
    for e in entries {
        let path = e.path();
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        println!(
            "  {}  ({size} bytes)",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        );
    }
}

fn clean(scratch_dir: &Path) {
    if !scratch_dir.exists() {
        println!("(no scratch directory at {})", scratch_dir.display());
        return;
    }
    if let Err(e) = fs::remove_dir_all(scratch_dir) {
        eprintln!("nmbrs report scratch clean: {e}");
        std::process::exit(2);
    }
    println!("removed {}", scratch_dir.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("nmbrs-scratch-{label}-{}", std::process::id(),));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn scratch_paths_for_plot_yields_md_plus_png() {
        let dir = temp_session("plot_paths");
        let item = ReportItem {
            kind: Kind::Plot,
            name: "demo".to_string(),
            ..Default::default()
        };
        let p = scratch_paths(&dir, &item).unwrap();
        assert_eq!(p.md, dir.join("scratch/demo.md"));
        assert_eq!(p.png, Some(dir.join("plot_demo.png")));
        assert!(dir.join("scratch").exists(), "scratch dir created");
    }

    #[test]
    fn scratch_paths_for_table_has_no_png() {
        let dir = temp_session("table_paths");
        let item = ReportItem {
            kind: Kind::Table,
            name: "summary".to_string(),
            ..Default::default()
        };
        let p = scratch_paths(&dir, &item).unwrap();
        assert_eq!(p.md, dir.join("scratch/summary.md"));
        assert!(p.png.is_none());
    }

    #[test]
    fn scratch_paths_for_text_has_no_png() {
        let dir = temp_session("text_paths");
        let item = ReportItem {
            kind: Kind::Text,
            name: "intro".to_string(),
            ..Default::default()
        };
        let p = scratch_paths(&dir, &item).unwrap();
        assert_eq!(p.md, dir.join("scratch/intro.md"));
        assert!(p.png.is_none());
    }

    #[test]
    fn timestamp_id_starts_with_year_and_has_hex_tail() {
        let id = timestamp_id();
        // YYYYMMDD prefix is 8 digits; underscore; HHmmss; underscore; 6 hex.
        assert_eq!(id.len(), 8 + 1 + 6 + 1 + 6);
        assert!(
            id[0..4].parse::<u32>().is_ok(),
            "year prefix should parse: {id}"
        );
        let tail = &id[16..22];
        assert!(
            tail.chars().all(|c| c.is_ascii_hexdigit()),
            "tail '{tail}' must be hex"
        );
    }

    #[test]
    fn timestamp_id_is_unique_in_loop() {
        // The PID×nonce tail should give different IDs even
        // when called in a tight loop. (It's allowed to
        // collide once per ~16M; statistically a tight loop
        // of 10 won't hit that.)
        let a = timestamp_id();
        let b = timestamp_id();
        let c = timestamp_id();
        // At least two of the three must differ, even if
        // the seconds-resolution prefix matches.
        assert!(a != b || b != c, "all three IDs collided: {a} {b} {c}");
    }

    #[test]
    fn list_handles_empty_scratch_dir_gracefully() {
        let dir = temp_session("list_empty");
        // No subcommand → list path; just exercise it
        // without panicking.
        list(&dir.join("scratch"));
    }

    #[test]
    fn clean_handles_missing_scratch_dir_gracefully() {
        let dir = temp_session("clean_missing");
        // Should print the "no scratch dir" message rather
        // than failing.
        clean(&dir.join("scratch"));
    }

    #[test]
    fn clean_removes_scratch_dir_with_contents() {
        let dir = temp_session("clean_with_contents");
        let scratch = dir.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("a.md"), "hello").unwrap();
        clean(&scratch);
        assert!(!scratch.exists());
    }
}
