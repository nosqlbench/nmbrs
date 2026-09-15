// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-46 unified `report:` block coverage tests.
//!
//! Each test runs `nmbrs/examples/workloads/metrics/reports_coverage.yaml`
//! through the two-step report pipeline:
//!
//!   1. `nmbrs run`       — populates metrics.db + standalone
//!      `<item>_summary.md` files for table items.
//!   2. `nmbrs report all` — injects text/table/file content into
//!      `summary.md` and creates sidecar `.md`
//!      files for items routed by `file` directives.
//!
//! Tests assert on the resulting markdown structure (sections,
//! anchors, tables, file routing). The session directory is
//! per-test (no shared state) so each test isolates against
//! prior test runs.
//!
//! Shapes covered (one assertion-focused test each):
//!
//! - Default group with a `table` item (figure-numbered + label)
//! - Multiple `report:` groups, each emitting a section
//! - `text` item — verbatim markdown prose
//! - `text` item with explicit `label "..."` heading
//! - `file` directive — subsequent items route to sidecar `.md`
//! - Standalone `<name>_summary.md` files written by `nmbrs run`
//! - Markdown table column headers from auto-discovered metrics

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/metrics/reports_coverage.yaml";

struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("nmbrs-reports-coverage-{pid}-{nanos}"));
        std::fs::create_dir_all(&parent).expect("create session parent");
        Self {
            path: parent.join("session"),
        }
    }
    fn parent(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.parent());
    }
}

/// Run the full two-step pipeline and return the session
/// directory. Panics if either step fails — the workload + run
/// shape is fixed, so a failure is a regression in the report
/// pipeline rather than a test-input issue.
fn run_and_report() -> SessionDir {
    let session = SessionDir::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    let run_out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={WORKLOAD}"))
        .arg("scenario=default")
        .output()
        .expect("invoke nmbrs run");
    assert!(
        run_out.status.success(),
        "nmbrs run failed: stderr=\n{}",
        String::from_utf8_lossy(&run_out.stderr),
    );

    let db = session.path.join("metrics.db");
    let report_out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(workspace_root)
        .arg("report")
        .arg("all")
        .arg(format!("workload={WORKLOAD}"))
        .arg("--db")
        .arg(&db)
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("invoke nmbrs report all");
    assert!(
        report_out.status.success(),
        "nmbrs report all failed: stderr=\n{}",
        String::from_utf8_lossy(&report_out.stderr),
    );

    session
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ─────────────────────────────────────────────────────────────────
// Standalone *_summary.md files (written by `nmbrs run`, no
// report-all step needed).
// ─────────────────────────────────────────────────────────────────

#[test]
fn reports_standalone_table_summary_files_exist() {
    // `nmbrs run` alone is enough — these files are written by the
    // runner inline, not by `nmbrs report all`.
    let session = SessionDir::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={WORKLOAD}"))
        .arg("scenario=default")
        .output()
        .expect("invoke nmbrs run");
    assert!(
        out.status.success(),
        "nmbrs run: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let default_table = session.path.join("default_table_summary.md");
    let latency_table = session.path.join("latency_table_summary.md");
    assert!(
        default_table.exists(),
        "expected default_table_summary.md at {}",
        default_table.display()
    );
    assert!(
        latency_table.exists(),
        "expected latency_table_summary.md at {}",
        latency_table.display()
    );
}

#[test]
fn reports_standalone_table_has_markdown_table_header() {
    // The auto-discovered phase table renders with the canonical
    // column header line. No `recall`/`latency` gauge exists in
    // this workload, so column selectors fall back to the
    // default columns (Activity, Cycles, Rate, p50, p99, mean).
    let session = run_and_report();
    let content = read_file(&session.path.join("default_table_summary.md"));
    assert!(
        content.contains("| Activity"),
        "missing Activity column: {content}"
    );
    assert!(
        content.contains("| Cycles"),
        "missing Cycles column: {content}"
    );
    assert!(content.contains("| p50"), "missing p50 column: {content}");
}

// ─────────────────────────────────────────────────────────────────
// summary.md sections — written by `nmbrs report all`.
// ─────────────────────────────────────────────────────────────────

#[test]
fn reports_summary_md_has_run_details_section() {
    // Auto-injected by the report pipeline. Anchored as
    // `run_details`.
    let session = run_and_report();
    let content = read_file(&session.path.join("summary.md"));
    assert!(
        content.contains("<a id=\"run_details\"></a>"),
        "missing run_details anchor: {content}"
    );
    assert!(
        content.contains("## Run Details"),
        "missing Run Details heading: {content}"
    );
}

#[test]
fn reports_text_item_with_label_overrides_heading() {
    // The `text` item declared `label "Coverage workload overview"`,
    // which becomes the section heading (not the canonical name).
    let session = run_and_report();
    let content = read_file(&session.path.join("summary.md"));
    assert!(
        content.contains("## Coverage workload overview (text)"),
        "text label should override default heading: {content}"
    );
    // The body is emitted verbatim (the literal `overview_intro`
    // identifier survives because the parser doesn't strip the
    // item name from a text body).
    assert!(
        content.contains("SRD-46 unified"),
        "text body content missing: {content}"
    );
}

#[test]
fn reports_default_group_table_anchored_with_figure_number() {
    // `default_table` is the first figure-eligible item → fig 1.
    let session = run_and_report();
    let content = read_file(&session.path.join("summary.md"));
    assert!(
        content.contains("<a id=\"default_table\"></a>"),
        "missing default_table anchor: {content}"
    );
    assert!(
        content.contains("## 1. Default table (table)"),
        "default_table figure heading missing: {content}"
    );
}

#[test]
fn reports_second_group_renders_as_distinct_section() {
    // `latency_table` lives in its own group (`latency_block`) —
    // confirms multi-group rendering each emits its own section.
    let session = run_and_report();
    let content = read_file(&session.path.join("summary.md"));
    assert!(
        content.contains("<a id=\"latency_table\"></a>"),
        "missing latency_table anchor: {content}"
    );
    assert!(
        content.contains("## 2. Latency table (table)"),
        "latency_table figure heading missing: {content}"
    );
}

// ─────────────────────────────────────────────────────────────────
// `file` directive — routes subsequent items to a sidecar file.
// ─────────────────────────────────────────────────────────────────

#[test]
fn reports_file_directive_creates_sidecar() {
    // `file detail.md` causes the text item declared after it
    // (`text_003`) to write to detail.md, NOT summary.md.
    let session = run_and_report();
    let sidecar = session.path.join("detail.md");
    assert!(
        sidecar.exists(),
        "expected sidecar detail.md at {}",
        sidecar.display()
    );
}

#[test]
fn reports_file_directive_routes_correct_item() {
    // `text_002` (declared BEFORE the `file` directive) lives in
    // summary.md. `text_003` (declared AFTER) lives in detail.md.
    let session = run_and_report();
    let summary = read_file(&session.path.join("summary.md"));
    let detail = read_file(&session.path.join("detail.md"));

    // Pre-`file` text section lands in summary.md.
    assert!(
        summary.contains("Items above this line live in summary.md."),
        "pre-file text not in summary.md: {summary}"
    );
    assert!(
        !detail.contains("Items above this line live in summary.md."),
        "pre-file text leaked into detail.md: {detail}"
    );

    // Post-`file` text section lands in detail.md.
    assert!(
        detail.contains("Items below the `file` directive live in detail.md."),
        "post-file text not in detail.md: {detail}"
    );
    assert!(
        !summary.contains("Items below the `file` directive live in detail.md."),
        "post-file text leaked into summary.md: {summary}"
    );
}

// ─────────────────────────────────────────────────────────────────
// `report list` surfaces every item with the correct kind tag.
// ─────────────────────────────────────────────────────────────────

#[test]
fn reports_list_surfaces_all_kinds() {
    // The list view enumerates every kind: text + table + file.
    // No execution needed beyond a single `nmbrs report list` —
    // doesn't require a prior `nmbrs run` since the report block
    // is parsed straight from the workload YAML.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(workspace_root)
        .arg("report")
        .arg("list")
        .arg(format!("workload={WORKLOAD}"))
        .output()
        .expect("invoke nmbrs report list");
    assert!(
        out.status.success(),
        "nmbrs report list failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Expect all six items in the listing — three text, two
    // tables, one file.
    assert!(stdout.contains("text"), "text kind missing: {stdout}");
    assert!(stdout.contains("table"), "table kind missing: {stdout}");
    assert!(stdout.contains("file"), "file kind missing: {stdout}");
    assert!(
        stdout.contains("default_table"),
        "default_table missing: {stdout}"
    );
    assert!(
        stdout.contains("latency_table"),
        "latency_table missing: {stdout}"
    );
    assert!(
        stdout.contains("detail.md"),
        "detail.md (file item) missing: {stdout}"
    );
}
