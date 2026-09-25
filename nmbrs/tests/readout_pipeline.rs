//! E2E test for the SRD-63 readout pipeline.
//!
//! Push 3 of the readouts implementation plan introduces
//! the workload `readouts:` block — a config surface that
//! replaces the hard-coded ✓ DONE / inline-status format
//! strings with workload-configurable readout bindings.
//! This test proves the full pipeline by:
//!
//! 1. Writing a tiny workload yaml that binds `trace` to
//!    `on_phase_end` (the smallest possible custom
//!    rendering — `trace` dumps every relevant
//!    `ReadoutContext` field).
//! 2. Spawning `nmbrs run` inside a `SteppableTerminal`
//!    so the rendered cells reach the test through a
//!    real PTY (matches operator-visible output, ANSI
//!    and all).
//! 3. Asserting the trace-readout output appears on
//!    screen — proving the workload yaml's binding made
//!    it through parse → bake → binder → render to the
//!    terminal.
//!
//! Push 5's stateful `TuiReadoutBinder` work will extend
//! this harness with key-handling assertions (`Tab` for
//! focus cycle, `+` for LOD cycle, `?`-held for the
//! explanation overlay).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::SteppableTerminal;

mod pty_support;

fn nmbrs_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nmbrs"))
}

/// Hand-rolled tempdir so the test stays self-contained;
/// cleans up on drop.
struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nmbrs-readout-pipeline-{pid}-{nanos}-{}",
            scratch::seq()
        ));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Write a workload that binds the `trace` readout at
/// `on_phase_end`. `trace` is a diagnostic readout (Push 2)
/// that surfaces every relevant ReadoutContext field as
/// labelled text — easy to grep for in the rendered cells.
///
/// Uses the `testkit` adapter, NOT `stdout`: per the SRD-41
/// console-ownership rule, `stdout` is a console-owning adapter
/// (`DisplayPreference::Off`) — on an interactive TTY it reserves
/// the console for its own field output and the `tui=terminal`
/// request is overridden to off, so readouts project to
/// `session.log` rather than the screen. `testkit` is `Auto`, so
/// `tui=terminal` is honored and the readout binder's output
/// renders to the terminal — which is exactly what this test
/// asserts.
fn trace_workload() -> (TempDir, PathBuf) {
    let dir = TempDir::new();
    let yaml_path = dir.path().join("trace.yaml");
    std::fs::write(
        &yaml_path,
        r#"readouts:
  on_phase_end: "trace"

scenarios:
  default: [run]

phases:
  run:
    adapter: testkit
    cycles: 3
    concurrency: 1
    bindings: |
      c := (cycle)
    ops:
      out:
        stmt: "x={c}"
"#,
    )
    .expect("write workload yaml");
    (dir, yaml_path)
}

fn build_config(workload: &Path, session: &Path) -> Config {
    let mut command: Vec<OsString> = Vec::new();
    command.push(nmbrs_binary().into());
    command.push("run".into());
    command.push(format!("workload={}", workload.display()).into());
    command.push("--session-path".into());
    command.push(session.into());
    command.push("tui=terminal".into());
    Config {
        width: 200,
        height: 60,
        command,
        scrollback_size: 2000,
        scrollback_step: 10,
    }
}

async fn assert_screen_contains(stepper: &mut SteppableTerminal, needle: &str, timeout: Duration) {
    pty_support::wait_for(stepper, needle, timeout).await;
}

/// SRD-63 §5: a workload-declared `readouts:` block routes
/// configured readout invocations through the binding layer
/// and into the terminal output. We bind `trace` (a
/// diagnostic readout that surfaces every context field) at
/// `on_phase_end` and assert its hallmark `event=on_phase_end`
/// header appears in the rendered cells.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workload_readouts_block_drives_terminal_output() {
    let (dir, yaml) = trace_workload();
    let session = dir.path().join("session");
    let config = build_config(&yaml, &session);

    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");

    // The trace readout's first emitted line for an
    // on_phase_end fire is `event=on_phase_end ...`. If the
    // binder didn't pick up the yaml binding, the default
    // `phase_outcome` ✓ line would fire instead and this
    // string never appears.
    assert_screen_contains(&mut stepper, "event=on_phase_end", Duration::from_secs(10)).await;
    assert_screen_contains(&mut stepper, "phase_name=\"run\"", Duration::from_secs(2)).await;
    assert_screen_contains(&mut stepper, "cycles=3/3", Duration::from_secs(2)).await;

    let _ = stepper.kill();
}

mod scratch;
