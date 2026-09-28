// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0
//
// These async tests serialize on a process-global lock held across
// `.await` so global session state doesn't race; the awaited code never
// locks it, so there's no deadlock — `await_holding_lock` is a false
// positive for this deliberate pattern.
#![allow(clippy::await_holding_lock)]

//! End-to-end coverage for SRD 23 §"TUI surface" — the inline
//! control-edit prompt. Tests drive [`App`] through its public
//! prompt API: open prompt, type characters, submit, parse
//! outcome, verify the underlying control value changed.
//!
//! The rendering itself is exercised via a TestBackend so the
//! prompt bar's wire format (input line + result line) stays
//! stable across refactors.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use nmbrs_metrics::component::Component;
use nmbrs_metrics::controls::{BranchScope, ControlBuilder, ControlOrigin};
use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::polydat_nodes::runtime_context::set_session_root;
use nmbrs_tui::app::App;
use nmbrs_tui::run_state_actor::spawn_run_state_actor;
use nmbrs_tui::state::RunState;

/// Serialize tests that mutate the process-global session root
/// (the same pattern used in runtime_context's own tests).
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_metrics_query() -> Arc<nmbrs_metrics::metrics_query::MetricsQuery> {
    use nmbrs_metrics::cadence::{CadenceTree, Cadences};
    use nmbrs_metrics::cadence_reporter::CadenceReporter;
    let tree = CadenceTree::plan_default(Cadences::defaults());
    let reporter = Arc::new(CadenceReporter::new(tree));
    let root = Component::root(Labels::of("session", "ce_test"), HashMap::new());
    Arc::new(nmbrs_metrics::metrics_query::MetricsQuery::new(
        reporter, root,
    ))
}

fn install_session_with_rate_control(initial: f64) -> Arc<RwLock<Component>> {
    let root = Component::root(
        Labels::empty()
            .with("type", "session")
            .with("session", "tui_edit"),
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

fn build_app() -> App {
    // The actor's join handle is dropped here; with no held
    // sender clones once the App drops its handle, the actor's
    // `recv()` errors out and the thread exits cleanly. Tests
    // don't observe the actor thread directly — they exercise
    // App's prompt API and snapshot reads.
    let (handle, _join) = spawn_run_state_actor(RunState::new("test.yaml", "smoke", "stdout"));
    let (_tx, rx) = mpsc::channel();
    App::new(rx, handle, test_metrics_query())
}

#[tokio::test]
async fn open_and_close_prompt_toggles_edit_mode() {
    let _g = TEST_LOCK.lock().unwrap();
    install_session_with_rate_control(100.0);
    let mut app = build_app();

    assert!(!app.is_editing(), "prompt should start closed");
    app.open_control_edit_prompt();
    assert!(app.is_editing());
    assert!(app.edit_prompt().unwrap().buffer.is_empty());
    app.close_control_edit_prompt();
    assert!(!app.is_editing());
}

#[tokio::test]
async fn edit_push_and_pop_char_mutate_buffer() {
    let _g = TEST_LOCK.lock().unwrap();
    install_session_with_rate_control(100.0);
    let mut app = build_app();
    app.open_control_edit_prompt();
    for c in "rate=500".chars() {
        app.edit_push_char(c);
    }
    assert_eq!(app.edit_prompt().unwrap().buffer, "rate=500");
    // Backspace drops the last character.
    app.edit_pop_char();
    assert_eq!(app.edit_prompt().unwrap().buffer, "rate=50");
}

#[tokio::test]
async fn submit_parses_name_value_and_writes_control() {
    let _g = TEST_LOCK.lock().unwrap();
    let root = install_session_with_rate_control(100.0);
    let control: nmbrs_metrics::controls::Control<f64> =
        root.read().unwrap().controls().get("rate").unwrap();

    let mut app = build_app();
    app.open_control_edit_prompt();
    for c in "rate=4242.0".chars() {
        app.edit_push_char(c);
    }
    let submitted = app.submit_control_edit();
    assert_eq!(submitted, Some(("rate".to_string(), 4242.0)));

    // The write is dispatched via tokio::spawn; yield until the
    // committed value catches up.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if control.value() == 4242.0 {
            break;
        }
    }
    assert_eq!(control.value(), 4242.0);
    assert_eq!(control.get().origin, ControlOrigin::Tui);

    // The prompt records success and clears its buffer for
    // follow-up edits.
    let p = app.edit_prompt().unwrap();
    assert!(p.buffer.is_empty());
    match &p.last_result {
        Some(Ok(msg)) => assert!(msg.contains("rate=4242")),
        other => panic!("expected Ok result, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_malformed_input_records_error_and_keeps_prompt_open() {
    let _g = TEST_LOCK.lock().unwrap();
    install_session_with_rate_control(100.0);
    let mut app = build_app();
    app.open_control_edit_prompt();
    for c in "no-equals-sign".chars() {
        app.edit_push_char(c);
    }
    assert!(app.submit_control_edit().is_none());
    let p = app.edit_prompt().unwrap();
    assert_eq!(p.buffer, "no-equals-sign", "buffer preserved on error");
    match &p.last_result {
        Some(Err(msg)) => assert!(msg.contains("name=value"), "got: {msg}"),
        other => panic!("expected parse error, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_non_f64_writable_control_errors_inline() {
    let _g = TEST_LOCK.lock().unwrap();
    // Declare a control WITHOUT from_f64; TUI writes should
    // error out with a clear message rather than silently drop.
    let root = Component::root(
        Labels::empty()
            .with("type", "session")
            .with("session", "nof"),
        HashMap::new(),
    );
    root.read().unwrap().controls().declare(
        ControlBuilder::new("rate", 100u32)
            .reify_as_gauge(|v| Some(*v as f64))
            .branch_scope(BranchScope::Subtree)
            .build(),
    );
    set_session_root(root);

    let mut app = build_app();
    app.open_control_edit_prompt();
    for c in "rate=250".chars() {
        app.edit_push_char(c);
    }
    app.submit_control_edit();
    let p = app.edit_prompt().unwrap();
    match &p.last_result {
        Some(Err(msg)) => assert!(
            msg.contains("not declared f64-writable") || msg.contains("from_f64"),
            "got: {msg}",
        ),
        other => panic!("expected f64-writable error, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_unknown_control_name_errors_inline() {
    let _g = TEST_LOCK.lock().unwrap();
    install_session_with_rate_control(100.0);
    let mut app = build_app();
    app.open_control_edit_prompt();
    for c in "nonexistent=1".chars() {
        app.edit_push_char(c);
    }
    app.submit_control_edit();
    let p = app.edit_prompt().unwrap();
    match &p.last_result {
        Some(Err(msg)) => assert!(msg.contains("nonexistent"), "got: {msg}",),
        other => panic!("expected unknown-name error, got {other:?}"),
    }
}

#[tokio::test]
async fn prompt_renders_input_and_result_lines() {
    let _g = TEST_LOCK.lock().unwrap();
    install_session_with_rate_control(100.0);
    let mut app = build_app();
    app.open_control_edit_prompt();
    for c in "rate=500".chars() {
        app.edit_push_char(c);
    }

    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();

    let buf = terminal.backend().buffer().clone();
    let text: String = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("edit control"),
        "prompt input line missing:\n{text}",
    );
    assert!(
        text.contains("rate=500"),
        "buffer contents missing from render:\n{text}",
    );
    assert!(
        text.contains("Enter submits") || text.contains("name=value"),
        "hint missing:\n{text}",
    );
}
