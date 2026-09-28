// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Layout rendering tests using ratatui's TestBackend.
//!
//! Renders the TUI into a headless terminal buffer and verifies
//! key elements appear in the right positions.

use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use nmbrs_metrics::summaries::binomial_summary::BinomialSummary;
use nmbrs_metrics::summaries::ewma::Ewma;
use nmbrs_metrics::summaries::peak_tracker::PeakTracker;
use nmbrs_tui::app::App;
use nmbrs_tui::run_state_actor::{RunStateHandle, spawn_run_state_actor};
use nmbrs_tui::state::{ActivePhase, RunState};

fn test_metrics_query() -> Arc<nmbrs_metrics::metrics_query::MetricsQuery> {
    use nmbrs_metrics::cadence::{CadenceTree, Cadences};
    use nmbrs_metrics::cadence_reporter::CadenceReporter;
    use nmbrs_metrics::component::Component;
    use nmbrs_metrics::labels::Labels;

    let tree = CadenceTree::plan_default(Cadences::defaults());
    let reporter = Arc::new(CadenceReporter::new(tree));
    let root = Component::root(
        Labels::of("session", "test"),
        std::collections::HashMap::new(),
    );
    Arc::new(nmbrs_metrics::metrics_query::MetricsQuery::new(
        reporter, root,
    ))
}

fn make_test_state() -> (RunStateHandle, JoinHandle<()>) {
    let mut state = RunState::new("full_cql_vector.yaml", "fknn_rampup", "cql");
    state.profiler = "off".into();
    state.limit = "5000".into();

    // Add some completed phases. No pre-mapped tree, so these route
    // by name via the runtime-materialized fallback (`scene_node_id =
    // 0` = root, never a Phase node → `find_phase`). SRD-100 P1c.
    state.set_phase_running(0, "teardown", "table=fknn_default", 3);
    state.set_phase_completed(
        0,
        "teardown",
        "table=fknn_default",
        0.2,
        nmbrs_tui::state::PhaseSummary::default(),
    );
    state.set_phase_running(0, "schema", "table=fknn_default", 4);
    state.set_phase_completed(
        0,
        "schema",
        "table=fknn_default",
        1.1,
        nmbrs_tui::state::PhaseSummary::default(),
    );

    // Active phase
    state.set_phase_running(0, "fknn_rampup_data", "optimize_for=RECALL", 1);
    let key = nmbrs_tui::state::ActivePhaseId::new(1, "fknn_rampup_data", "optimize_for=RECALL");
    state.active_phases.insert(
        key,
        ActivePhase {
            name: "fknn_rampup_data".into(),
            labels: "optimize_for=RECALL".into(),
            cursor_name: "row".into(),
            cursor_extent: 5000,
            daemon: false,
            rows_consumed: 2500,
            rows_total: 5000,
            fibers: 100,
            started_at: Instant::now(),
            session_started: 0.0,
            ops_started: 2600,
            ops_finished: 2500,
            ops_ok: 2500,
            skips: 0,
            errors: 0,
            retries: 0,
            ops_per_sec: 220.0,
            adapter_counters: vec![("rows_inserted".into(), 19500, 1700.0)],
            rows_per_batch: 7.8,
            relevancy: Vec::new(),
            throughput_summary: Arc::new(BinomialSummary::new(60)),
            rate_ewma: Arc::new(Ewma::new(Duration::from_secs(5))),
            latency_peak_5s: Arc::new(PeakTracker::max(Duration::from_secs(5))),
            latency_peak_10s: Arc::new(PeakTracker::max(Duration::from_secs(10))),
            render: None,
        },
    );

    // Pending phases
    state.add_phase("pvs_query", "k=10", 0);
    state.add_phase("pvs_query", "k=100", 0);

    // Latency
    state.p50_nanos = 1_200_000; // 1.2ms
    state.p90_nanos = 3_800_000; // 3.8ms
    state.p99_nanos = 12_400_000; // 12.4ms
    state.p999_nanos = 45_100_000; // 45.1ms
    state.max_nanos = 89_200_000; // 89.2ms

    // Sparkline history
    for i in 0..30 {
        state.push_ops_sample(180.0 + i as f64 * 1.5);
        state.push_rows_sample(1400.0 + i as f64 * 12.0);
    }

    spawn_run_state_actor(state)
}

// `#[tokio::test]` (not `#[test]`): `App::new` builds a `MetricsQuery`
// whose `CadenceReporter::new` spawns its single-writer actor as a tokio
// task, so a runtime must be active at construction (same pattern as
// control_edit_test). The render body itself is synchronous.
#[tokio::test]
async fn render_layout_has_all_sections() {
    let (state, _actor_join) = make_test_state();
    let (_tx, rx) = mpsc::channel();
    let mut app = App::new(rx, state.clone(), test_metrics_query());
    // Maximal LOD expands the detail block (cursor, concurrency,
    // latency, sparklines) under every phase; this test asserts on
    // those sections specifically, so force the LOD rather than
    // running against Default which hides them.
    app.set_tree_lod_label("max");

    // Height needs to fit all phase detail blocks + pending phases
    // below them; 60 rows comfortably covers every section the test
    // asserts on.
    let backend = TestBackend::new(120, 60);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| app.draw(frame)).unwrap();

    let buf = terminal.backend().buffer().clone();
    let text: String = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Header section
    assert!(text.contains("nmbrs"), "missing header title:\n{text}");
    assert!(
        text.contains("full_cql_vector.yaml"),
        "missing workload name:\n{text}"
    );
    assert!(
        text.contains("fknn_rampup"),
        "missing scenario name:\n{text}"
    );

    // Phase panel
    assert!(
        text.contains("fknn_rampup_data"),
        "missing active phase name:\n{text}"
    );
    assert!(text.contains("cursor row"), "missing cursor name:\n{text}");
    assert!(
        text.contains("concurrency: 100"),
        "missing concurrency count:\n{text}"
    );
    assert!(
        text.contains("active: 100"),
        "missing active count:\n{text}"
    );

    // Latency section
    assert!(text.contains("p50"), "missing p50 label:\n{text}");
    assert!(text.contains("p90"), "missing p90 label:\n{text}");
    assert!(
        text.contains("1.20ms"),
        "missing p50 value (1.20ms):\n{text}"
    );

    // Sparkline / rate section — rendered as "rows/s" in the
    // detail block.
    assert!(text.contains("rows/s"), "missing rows/s label:\n{text}");

    // Scenario tree. The Running marker is now an animated
    // Braille spinner, so we accept any of its frames rather
    // than the old static `▶`.
    use throbber_widgets_tui::symbols::throbber::BRAILLE_SIX;
    assert!(
        text.contains("✓"),
        "missing completed phase marker:\n{text}"
    );
    assert!(
        BRAILLE_SIX.symbols.iter().any(|s| text.contains(s)),
        "missing running phase spinner frame (any of {:?}):\n{text}",
        BRAILLE_SIX.symbols,
    );
    assert!(text.contains("○"), "missing pending phase marker:\n{text}");
    assert!(
        text.contains("teardown"),
        "missing teardown in tree:\n{text}"
    );
    assert!(
        text.contains("pvs_query"),
        "missing pvs_query in tree:\n{text}"
    );

    // Footer
    assert!(text.contains("quit"), "missing quit hint:\n{text}");
}

#[tokio::test]
async fn render_layout_no_active_phase() {
    // At least one pending phase must exist for the tree to have
    // a "nothing running yet" state (rather than "scenario
    // complete"). The placeholder surfaces in Focus LOD when
    // phases exist but none are Running.
    let mut rs = RunState::new("test.yaml", "smoke", "stdout");
    rs.add_phase("init", "", 0);
    let (state, _actor_join) = spawn_run_state_actor(rs);

    let (_tx, rx) = mpsc::channel();
    let mut app = App::new(rx, state, test_metrics_query());
    app.set_tree_lod_label("focus");

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| app.draw(frame)).unwrap();

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
        text.contains("waiting"),
        "should show 'waiting' when no active phase:\n{text}"
    );
    assert!(
        text.contains("nmbrs"),
        "header should always render:\n{text}"
    );
}

#[tokio::test]
async fn render_layout_narrow_terminal() {
    let (state, _actor_join) = make_test_state();
    let (_tx, rx) = mpsc::channel();
    let app = App::new(rx, state, test_metrics_query());

    // Narrow terminal — should not panic
    let backend = TestBackend::new(40, 15);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    // Just verify it rendered without panicking
    let buf = terminal.backend().buffer().clone();
    assert!(buf.area.width == 40);
}

#[tokio::test]
async fn render_prints_full_buffer() {
    let (state, _actor_join) = make_test_state();
    let (_tx, rx) = mpsc::channel();
    let app = App::new(rx, state, test_metrics_query());

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let buf = terminal.backend().buffer().clone();
    eprintln!("=== TUI Render (120x35) ===");
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        eprintln!("{}", line.trim_end());
    }
    eprintln!("=== END ===");
}
