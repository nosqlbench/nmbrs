// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! C9 — phase-level `metrics:` honor `cell:`/`dimensions:` placement,
//! over `nmbrs/examples/workloads/metrics/phase_cell_metrics.yaml`.
//!
//! One placement rule across the op and phase tiers: a cell-placed
//! phase metric registers on the coordinate's cell component, so its
//! persisted instance spec carries the dimension label
//! (`tier="gold"`) instead of landing bare on the phase component.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/metrics/phase_cell_metrics.yaml";

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("nmbrs-phasecell-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn phase_metric_lands_at_its_declared_cell() {
    let sandbox = Sandbox::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg("scenario=default")
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    let out = cmd.output().expect("run nmbrs");
    let evidence = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(session.join("session.log")).unwrap_or_default(),
    );
    assert!(
        out.status.success(),
        "run must complete; evidence:\n{evidence}"
    );

    // The instance spec is the OpenMetrics-canonical `name{k="v",…}`
    // rendering — the cell's dimension label must be part of the
    // recorded identity.
    let conn =
        rusqlite::Connection::open(session.join("metrics.db")).expect("open session metrics.db");
    let spec: String = conn
        .query_row(
            "SELECT mi.spec FROM metric_instance mi \
             JOIN metric_family mf ON mi.family_id = mf.id \
             WHERE mf.name LIKE 'stamped_ms%'",
            [],
            |r| r.get(0),
        )
        .expect("stamped_ms instance row (was the metric emitted at all?)");
    assert!(
        spec.contains("tier=\"gold\""),
        "cell-placed phase metric must carry its coordinate label; spec: {spec}"
    );
}
