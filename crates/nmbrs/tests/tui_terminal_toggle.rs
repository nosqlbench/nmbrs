//! E2E tests for the `tui=terminal` ↔ `tui=on` toggle path.
//!
//! The toggle is interactive: Ctrl-T from terminal mode swaps
//! to the full TUI; Ctrl-T (or `q`) inside the TUI swaps back.
//! There's no piped-output surface that exposes whether the
//! swap actually happened — both modes write to stderr, just
//! with different ANSI dressing.
//!
//! [`shadow_terminal::SteppableTerminal`] gives us an in-memory,
//! fully-rendered terminal we can drive: spawn `nmbrs run` as
//! a child, send keystrokes through the PTY, and read back the
//! rendered cells. That's enough to distinguish "line-mode log
//! lines" from "alt-screen TUI" by content.
//!
//! These tests build their own workload file in `tempdir` so
//! they don't depend on workspace fixtures and stay
//! self-contained.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::{Input, SteppableTerminal};

mod pty_support;

/// Path to the `nmbrs` binary cargo built for this test.
/// `CARGO_BIN_EXE_<name>` is populated during integration-test
/// builds for binaries declared in the same crate.
fn nmbrs_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nmbrs"))
}

/// Write a tiny stdout-adapter workload that runs slowly enough
/// (rate-limited) for the test to observe both display modes
/// before completion. Returns the path inside a fresh tempdir
/// so the test never collides with sibling runs.
fn slow_stdout_workload() -> (TempDir, PathBuf) {
    let dir = TempDir::new();
    let yaml_path = dir.path().join("toggle.yaml");
    std::fs::write(
        &yaml_path,
        // `rate:` paces the op (rate wrapper) so the 20-cycle run takes a
        // couple of seconds rather than ~50ms — otherwise the tick lines scroll
        // the startup banner off the 30-row screen before the stepper can
        // observe it (the fixture's "runs slowly enough" comment requires it).
        r#"ops:
  hello:
    rate: 10
    raw: "tick={cycle}"
"#,
    )
    .expect("write workload yaml");
    (dir, yaml_path)
}

/// A workload whose op paces itself via its own `rate:` field so
/// the single synthetic phase `main` stays Running for the whole
/// observation window. Callers drive it with `filename=/dev/null`
/// so the stdout adapter writes nothing to the terminal — no
/// second writer fighting the sink's absolutely-positioned region.
fn paced_silent_workload() -> (TempDir, PathBuf) {
    let dir = TempDir::new();
    let yaml_path = dir.path().join("paced.yaml");
    std::fs::write(
        &yaml_path,
        r#"ops:
  hello:
    raw: "tick={cycle}"
    rate: "5/s"
"#,
    )
    .expect("write paced workload yaml");
    (dir, yaml_path)
}

/// Hand-rolled tempdir so we don't add a `tempfile` dep just
/// for these tests. Cleans up on drop.
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
            "nmbrs-toggle-test-{pid}-{nanos}-{}",
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

/// Build the steppable-terminal config for `nmbrs run`. Callers
/// pass `cycles=` / `rate=` (and anything else) in `extra` — they
/// are NOT hardcoded here, so a caller can pick a rate/duration
/// without a duplicate param (two `rate=` values trip the
/// op-rate-wrapper field-ownership validation).
///
/// Session output is redirected into the workload's tempdir via
/// `--session-path` so the run never writes session directories
/// under the project root (see the project's test-isolation
/// memory).
fn build_config(workload: &Path, extra: &[&str]) -> Config {
    let sessions = workload
        .parent()
        .expect("workload path has a parent tempdir")
        .join("sessions");
    let mut command: Vec<OsString> = Vec::new();
    command.push(nmbrs_binary().into());
    command.push("run".into());
    command.push(format!("workload={}", workload.display()).into());
    command.push("--session-path".into());
    command.push(sessions.into());
    command.push("tui=terminal".into());
    for arg in extra {
        command.push((*arg).into());
    }
    Config {
        width: 120,
        height: 30,
        command,
        scrollback_size: 500,
        scrollback_step: 5,
    }
}

/// Step the terminal until a substring shows up on screen, or
/// kill the child and panic at the deadline (shared drive loop —
/// see `pty_support`).
async fn assert_screen_contains(stepper: &mut SteppableTerminal, needle: &str, timeout: Duration) {
    pty_support::wait_for(stepper, needle, timeout).await;
}

/// `tui=terminal` in a real PTY produces line-mode output
/// (no alt-screen). We assert that a phase-start line is
/// observable on screen, which confirms the
/// `LogOnlyObserver` + `LogOnlySink` pipeline is rendering
/// correctly through the actor + snapshot drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_mode_renders_log_lines() {
    let (_dir, yaml) = slow_stdout_workload();
    let config = build_config(&yaml, &["cycles=20"]);
    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");

    // The canonical "rendering works" tell: the runner's
    // startup banner ("1 ops, N cycles, concurrency=1, …")
    // reaches the screen via observer::log → actor →
    // LogOnlySink → PTY → shadow terminal. If we see it,
    // every layer is wired.
    assert_screen_contains(&mut stepper, "ops, ", Duration::from_secs(8)).await;

    let _ = stepper.kill();
}

/// Drive the Ctrl-T toggle terminal → full TUI → terminal.
///
/// The former race (Ctrl-T landing before the runner published a
/// `MetricsQuery`, so the supervisor declined the swap) is gated
/// out by construction: we wait for the live status block (its
/// `ok:` field) before sending Ctrl-T. The status block only
/// renders once the phase is `Running` and the inline-status
/// thread has refreshed, which is *after* the runner's
/// `observer.on_metrics_query(...)` in the execution-setup block
/// (runner.rs) — so by the time `ok:` is on screen the query the
/// `TuiSink` needs is already wired. (There is no managed
/// phase-history region anymore; completed phases live in the
/// scrollback and the running phase shows only in the live status
/// block, which is re-derived from the snapshot on every sink.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_t_toggles_into_tui_and_back() {
    let (_dir, yaml) = paced_silent_workload();
    let config = build_config(&yaml, &["cycles=300", "filename=/dev/null"]);
    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");

    // Terminal mode renders the running phase as the live status
    // block; its `ok:` field also signals the MetricsQuery is
    // published.
    assert_screen_contains(&mut stepper, "ok:", Duration::from_secs(10)).await;

    // Ctrl-T (ASCII 0x14): the watcher forwards ToggleTui and the
    // supervisor swaps in the TuiSink (alt-screen). The TUI draws
    // bordered panels with box-drawing the line-mode sink never
    // emits — `┌` (a panel corner) is a clean alt-screen tell.
    stepper
        .send_input(Input::Characters("\x14".into()))
        .expect("send Ctrl-T");
    assert_screen_contains(&mut stepper, "┌", Duration::from_secs(8)).await;

    // Ctrl-T back: the App sets `yielded_to_terminal`, the
    // supervisor brings the LogOnlySink back up, and the live
    // status block re-derives from the snapshot — `ok:` returns.
    stepper
        .send_input(Input::Characters("\x14".into()))
        .expect("send Ctrl-T (back)");
    assert_screen_contains(&mut stepper, "ok:", Duration::from_secs(8)).await;

    let _ = stepper.kill();
}

mod scratch;
