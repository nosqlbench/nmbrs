// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0
//
// These async tests serialize on a process-global lock held across
// `.await` so global session state doesn't race; the awaited code never
// locks it, so there's no deadlock — `await_holding_lock` is a false
// positive for this deliberate pattern.
#![allow(clippy::await_holding_lock)]

//! End-to-end test for the inspector server: spawn the
//! actor + server, send commands over the Unix socket, verify
//! the wire responses. Exercises the same code path
//! `nmbrs --inspector` uses, without needing a real TTY.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nmbrs_tui::inspector_repl;
use nmbrs_tui::inspector_server;
use nmbrs_tui::run_state_actor::{RunStateCmd, spawn_run_state_actor};
use nmbrs_tui::state::RunState;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, RwLock};

use nmbrs_metrics::component::Component;
use nmbrs_metrics::controls::{BranchScope, ControlBuilder};
use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::polydat_nodes::runtime_context::set_session_root;

/// Serialize tests that mutate the process-global session root —
/// matches the pattern used in `control_edit_test.rs`.
static SESSION_ROOT_LOCK: Mutex<()> = Mutex::new(());

fn install_session_with_rate_control(initial: f64) -> Arc<RwLock<Component>> {
    let root = Component::root(
        Labels::empty()
            .with("type", "session")
            .with("session", "inspector_test"),
        HashMap::new(),
    );
    root.read().unwrap().controls().declare(
        ControlBuilder::new("rate", initial)
            .reify_as_gauge(|v: &f64| Some(*v))
            .from_f64(|v| {
                if v <= 0.0 {
                    Err(format!("rate must be positive, got {v}"))
                } else {
                    Ok(v)
                }
            })
            .branch_scope(BranchScope::Subtree)
            .build(),
    );
    set_session_root(root.clone());
    root
}

/// Unique-per-call socket path under /tmp so parallel test
/// invocations don't collide. The pid + a process-local
/// counter gives us deterministic uniqueness even when
/// `cargo test` runs the same test multiple times.
fn tempfile_path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    PathBuf::from(format!("/tmp/nmbrs-test-{}-{}.sock", std::process::id(), n))
}

#[test]
fn server_responds_to_basic_commands() {
    let mut rs = RunState::new("test.yaml", "smoke", "stdout");
    rs.profiler = "off".into();
    rs.limit = "100".into();
    // By-name fallback (no pre-mapped tree): root id 0 → find_phase. SRD-100 P1c.
    rs.set_phase_running(0, "schema", "", 4);

    let (handle, _join) = spawn_run_state_actor(rs);
    let tmp = tempfile_path();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), None).expect("inspector should bind");

    // Give the listener a moment to begin accepting.
    std::thread::sleep(Duration::from_millis(20));

    let commands = inspector_repl::query(&sock_path, "commands").unwrap();
    let names: Vec<&str> = commands.lines().collect();
    assert!(names.contains(&"meta"));
    assert!(names.contains(&"phases"));
    assert!(names.contains(&"snapshot"));
    assert!(names.contains(&"log"));

    let meta = inspector_repl::query(&sock_path, "meta").unwrap();
    assert!(
        meta.contains("workload : test.yaml"),
        "meta missing workload:\n{meta}"
    );
    assert!(meta.contains("scenario : smoke"));
    assert!(meta.contains("limit    : 100"));

    let phases = inspector_repl::query(&sock_path, "phases").unwrap();
    assert!(phases.contains("schema"), "phases missing entry:\n{phases}");

    let pid = inspector_repl::query(&sock_path, "pid").unwrap();
    let pid: u32 = pid.trim().parse().unwrap();
    assert_eq!(pid, std::process::id());

    let unknown = inspector_repl::query(&sock_path, "totally-not-a-command").unwrap();
    assert!(
        unknown.starts_with("ERR"),
        "unknown command should error:\n{unknown}"
    );

    // Cleanup: the actor and server shut down when their handle
    // clones drop; the test process exit takes care of the rest.
    drop(handle);
}

#[test]
fn server_reflects_live_state_changes() {
    let (handle, _actor_join) =
        spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let tmp = tempfile_path();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), None).expect("inspector should bind");
    std::thread::sleep(Duration::from_millis(20));

    // Initially no phases.
    let p = inspector_repl::query(&sock_path, "phases").unwrap();
    assert!(p.contains("(no phases)"), "expected empty phases:\n{p}");

    // Send a command — the actor publishes a new snapshot,
    // and the next inspector query sees it.
    handle.send(RunStateCmd::PhaseStarting {
        exec_id: 1,
        scene_node_id: 0,
        name: "ramp".into(),
        labels: "k=10".into(),
        op_templates: 100,
        total_cycles: 100,
        concurrency: 8,
    });

    // Spin until the snapshot reflects the phase, capped at
    // 2s. The actor processes commands on its own thread; in
    // practice this is sub-millisecond.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got = String::new();
    while Instant::now() < deadline {
        got = inspector_repl::query(&sock_path, "phases").unwrap();
        if got.contains("ramp") {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(got.contains("ramp"), "phase never reached snapshot:\n{got}");
    assert!(got.contains("k=10"));
}

#[test]
fn server_lists_help_with_new_commands() {
    // The new commands (controls/set/metrics/metric) must show
    // up in `commands` and `help` so REPL autocomplete and
    // operators discover them.
    let (handle, _join) = spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let tmp = tempfile_path();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), None).expect("inspector should bind");
    std::thread::sleep(Duration::from_millis(20));

    let commands = inspector_repl::query(&sock_path, "commands").unwrap();
    let names: Vec<&str> = commands.lines().collect();
    assert!(names.contains(&"controls"));
    assert!(names.contains(&"set"));
    assert!(names.contains(&"metrics"));
    assert!(names.contains(&"metric"));

    let help = inspector_repl::query(&sock_path, "help").unwrap();
    assert!(help.contains("controls"));
    assert!(help.contains("set <name>"));
    assert!(help.contains("metric <selector>"));

    drop(handle);
}

#[test]
fn set_without_runtime_handle_errors_cleanly() {
    // Without a captured tokio runtime the server must still
    // respond — just rejecting writes with a structured error
    // so REPL clients see a recognizable code.
    let (handle, _join) = spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let tmp = tempfile_path();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), None).expect("inspector should bind");
    std::thread::sleep(Duration::from_millis(20));

    let resp = inspector_repl::query(&sock_path, "set concurrency 32").unwrap();
    assert!(
        resp.starts_with("ERR no_runtime"),
        "expected ERR no_runtime, got: {resp}",
    );

    // Bad parse before runtime check still errors at parse stage.
    let resp = inspector_repl::query(&sock_path, "set").unwrap();
    assert!(resp.starts_with("ERR parse"), "got: {resp}");

    drop(handle);
}

#[test]
fn metric_command_parses_selector_arguments() {
    // Selector parser rejects malformed input regardless of
    // whether a session root is installed. (Other tests cover
    // the no-session path; this one only validates parse
    // errors so it doesn't race with parallel tests that
    // install a session root.)
    let (handle, _join) = spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let tmp = tempfile_path();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), None).expect("inspector should bind");
    std::thread::sleep(Duration::from_millis(20));

    // Bad selector → ERR parse, regardless of session presence.
    let bad = inspector_repl::query(&sock_path, "metric foo{bad}").unwrap();
    assert!(bad.starts_with("ERR parse"), "got: {bad}");

    let bad = inspector_repl::query(&sock_path, "metric foo{=v}").unwrap();
    assert!(bad.starts_with("ERR parse"), "got: {bad}");

    let bad = inspector_repl::query(&sock_path, "metric").unwrap();
    assert!(bad.starts_with("ERR parse"), "got: {bad}");

    drop(handle);
}

#[test]
fn controls_lists_declared_controls_through_session_root() {
    let _g = SESSION_ROOT_LOCK.lock().unwrap();
    let _root = install_session_with_rate_control(123.5);

    let (handle, _join) = spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let tmp = tempfile_path();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), None).expect("inspector should bind");
    std::thread::sleep(Duration::from_millis(20));

    let resp = inspector_repl::query(&sock_path, "controls").unwrap();
    assert!(resp.contains("rate"), "expected rate control row:\n{resp}");
    assert!(
        resp.contains("scope=subtree"),
        "expected subtree scope:\n{resp}"
    );

    drop(handle);
}

#[tokio::test]
async fn set_writes_through_runtime_handle() {
    let _g = SESSION_ROOT_LOCK.lock().unwrap();
    let root = install_session_with_rate_control(100.0);
    let control: nmbrs_metrics::controls::Control<f64> =
        root.read().unwrap().controls().get("rate").unwrap();

    let (handle, _join) = spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let tmp = tempfile_path();
    let runtime_handle = tokio::runtime::Handle::current();
    let (sock_path, _server_join) =
        inspector_server::spawn_at(tmp, handle.clone(), Some(runtime_handle))
            .expect("inspector should bind");
    std::thread::sleep(Duration::from_millis(20));

    // The query is sync from the test thread; it issues a
    // single-line request, the server-side `block_on` runs the
    // async write to completion before responding.
    let resp = tokio::task::spawn_blocking({
        let sock_path = sock_path.clone();
        move || inspector_repl::query(&sock_path, "set rate 250").unwrap()
    })
    .await
    .unwrap();

    assert!(resp.starts_with("OK rate"), "expected OK, got: {resp}");
    assert!(resp.contains("value=250"), "got: {resp}");
    assert_eq!(control.value(), 250.0);

    // Validation failures surface as ERR validation_failed.
    let resp = tokio::task::spawn_blocking({
        let sock_path = sock_path.clone();
        move || inspector_repl::query(&sock_path, "set rate -1").unwrap()
    })
    .await
    .unwrap();
    assert!(
        resp.starts_with("ERR validation_failed"),
        "expected validation error, got: {resp}"
    );

    // Unknown control name → ERR not_found.
    let resp = tokio::task::spawn_blocking({
        let sock_path = sock_path.clone();
        move || inspector_repl::query(&sock_path, "set nonexistent 1.0").unwrap()
    })
    .await
    .unwrap();
    assert!(
        resp.starts_with("ERR not_found"),
        "expected not_found, got: {resp}"
    );

    drop(handle);
}
