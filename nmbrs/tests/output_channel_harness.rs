// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-87 Output Channel — behavioral regression harness.
//!
//! Pins the user-visible output behavior across the three terminal
//! contexts the channel selects between — **piped**, **console-owning**
//! (an adapter owns the screen on a TTY), and the **interactive
//! dashboard** — so the Stage C sink fold (SRD-87 §11) can be verified
//! not to regress them. The in-process bucket-routing pins live in the
//! `nmbrs-runtime::output_channel` unit tests; these are the end-to-end
//! surface pins.
//!
//! The PTY pins use [`shadow_terminal::steppable_terminal::SteppableTerminal`]
//! — an in-memory wezterm emulator, the same harness as
//! `plotter_alignment.rs`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::SteppableTerminal;

mod pty_support;
use pty_support::{settle, wait_for};

fn nmbrs_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nmbrs"))
}

/// Tempdir for the workload + `--session-path`, cleaned on drop.
/// `std::env::temp_dir()` is redirected under `target/` by
/// `.cargo/config.toml`.
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
        let path =
            std::env::temp_dir().join(format!("nmbrs-outch-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Build a `SteppableTerminal` config running `nmbrs run <args...>` with a
/// `--session-path` under `sessions`.
fn pty_config(args: &[&str], sessions: &Path, width: u16, height: u16) -> Config {
    let mut command: Vec<OsString> = vec![nmbrs_binary().into(), "run".into()];
    for a in args {
        command.push((*a).into());
    }
    command.push("--session-path".into());
    command.push(sessions.into());
    Config {
        width,
        height,
        command,
        scrollback_size: 500,
        scrollback_step: 5,
    }
}

/// Markers of the run's diagnostic/narration plane (the log + banners).
const DIAG_MARKERS: &[&str] = &["1 phases", "session:", "adapter=", "phase '", "metrics:"];

// ---------------------------------------------------------------------
// 1. Piped — robust, no PTY. The data plane is on stdout; the diagnostic
//    plane is on stderr; the two never mix.
// ---------------------------------------------------------------------

#[test]
fn piped_op_output_is_exactly_the_lines_and_diagnostics_go_to_stderr() {
    let tmp = TempDir::new();
    let out = Command::new(nmbrs_binary())
        .args([
            "run",
            "op=id-{cycle}",
            "cycles=5",
            "adapter=stdout",
            "--session-path",
        ])
        .arg(&tmp.path)
        .output()
        .expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec!["id-0", "id-1", "id-2", "id-3", "id-4"],
        "piped op-output must be exactly the rendered lines on stdout; got: {stdout:?}"
    );
    assert!(
        DIAG_MARKERS.iter().any(|m| stderr.contains(m)),
        "diagnostics must reach stderr when piped; stderr: {stderr:?}"
    );
    // The data plane must NOT carry diagnostics.
    for m in DIAG_MARKERS {
        assert!(
            !stdout.contains(m),
            "diagnostic {m:?} leaked onto piped stdout: {stdout:?}"
        );
    }
}

// ---------------------------------------------------------------------
// 2. Console-owning on a TTY — the adapter owns the screen: op output
//    prints, diagnostics are suppressed to session.log.
// ---------------------------------------------------------------------

// shadow-terminal PTY test. Serialized under nextest via the `pty` test-group
// (`.config/nextest.toml`) so concurrent PTY allocation can't stall it; runs
// normally under plain `cargo test`. A global `slow-timeout` is the backstop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_owning_tty_prints_op_output_with_no_diagnostic_leak() {
    let tmp = TempDir::new();
    let sessions = tmp.path.join("s");
    // `rate=` paces emission so the emulator drains each raw line before the
    // (otherwise instant) child exits — a harness concern, not behavior.
    let cfg = pty_config(
        &["op=id-{cycle}", "cycles=10", "adapter=stdout", "rate=20"],
        &sessions,
        110,
        40,
    );
    let mut stepper = SteppableTerminal::start(cfg).await.expect("start pty");
    // Wait for the LAST line so the emulator has drained the whole run.
    wait_for(&mut stepper, "id-9", Duration::from_secs(20)).await;
    settle(&mut stepper).await;
    let screen = stepper.screen_as_string().expect("screen");

    // Every op line is on the owned console.
    for i in 0..10 {
        assert!(
            screen.contains(&format!("id-{i}")),
            "console-owning op line id-{i} missing; screen:\n{screen}"
        );
    }
    // The run's diagnostics are suppressed — none on the screen.
    for line in screen.lines() {
        for m in DIAG_MARKERS {
            assert!(
                !line.contains(m),
                "diagnostic {m:?} leaked onto the console-owning screen: {line:?}"
            );
        }
    }
    // ...but they WERE captured to session.log (intake preserved).
    let log = std::fs::read_to_string(sessions.join("session.log")).unwrap_or_default();
    assert!(
        log.contains("phase") || log.contains("adapter"),
        "diagnostics should be captured in session.log; got:\n{log}"
    );
}

// ---------------------------------------------------------------------
// 3. Interactive dashboard — a workload run (no CLI adapter=) drives the
//    LogOnlySink, which renders the run outcome.
// ---------------------------------------------------------------------

// shadow-terminal PTY test — serialized under nextest via the `pty`
// test-group (`.config/nextest.toml`); runs normally under `cargo test`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_dashboard_renders_the_run() {
    let tmp = TempDir::new();
    let sessions = tmp.path.join("s");
    let wl = tmp.path.join("dash.yaml");
    std::fs::write(
        &wl,
        "bindings: |\n  input cycle: u64\nops:\n  q:\n    stmt: \"row-{cycle}\"\n",
    )
    .expect("write workload");
    let wl_arg = format!("workload={}", wl.display());
    // A short run so the whole render (rows + outcome) fits the screen
    // without scrolling the outcome out of view. `rate=` paces emission
    // so the emulator drains each rendered line before the (otherwise
    // instant) child exits — without it the child closes the PTY before
    // shadow_terminal finishes draining and `render_all_output()` blocks
    // (same harness concern the console-owning pin handles). Behavioral
    // assertions below are unaffected by the pacing.
    let cfg = pty_config(&[&wl_arg, "cycles=30", "rate=20"], &sessions, 110, 40);
    let mut stepper = SteppableTerminal::start(cfg).await.expect("start pty");
    // Wait for the last row so the sink has rendered through the end.
    wait_for(&mut stepper, "row-29", Duration::from_secs(30)).await;
    settle(&mut stepper).await;
    let screen = stepper.screen_as_string().expect("screen");
    // The dashboard composites op output behind its live status frame — the
    // `│` separator is the LogOnlySink's signature (absent in raw output).
    assert!(
        screen.contains('│'),
        "dashboard must render its live status frame (the `│` prefix); screen:\n{screen}"
    );
    // ...and the run outcome (✓ / post-run summary) renders through the sink.
    assert!(
        screen.contains("completed") || screen.contains('✓'),
        "dashboard must render the run outcome; screen:\n{screen}"
    );
}

mod scratch;
