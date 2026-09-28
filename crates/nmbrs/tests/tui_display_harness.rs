//! Fully-encapsulated E2E tests for the `tui=terminal` display
//! sink's **console containment** and **surface restore** across
//! display-mode transitions.
//!
//! These do NOT run a real workload. They spawn the
//! `tui_display_harness` example — which drives the real display
//! sink ([`nmbrs_tui::log_only_sink::LogOnlySink`]) from a mock
//! data source (the run-state actor) — under a
//! [`shadow_terminal`] PTY, and lock-step it: send one state
//! change over stdin, wait for the rendered cells, assert, send
//! the next.
//!
//! Completed-phase history is NOT a managed screen region (that
//! stateful-buffer strategy was removed): it lives in the
//! scrollback as ordinary log lines, with the terminal's own
//! alternate-screen save/restore preserving it across mode swaps.
//! So these tests anchor on plain `log` lines as the stable
//! primary surface and assert the console (REPL) frame opens on
//! the alternate screen and restores the primary byte-exact on
//! close — never leaking its output onto the primary scrollback.

// Unix-only: this suite lock-steps the harness over PTY stdin and
// asserts byte-exact screen state. Under Windows ConPTY the input
// path is cooked by the console host (line buffering, echo onto
// the surface, CR-vs-LF Enter semantics), which breaks the
// lock-step contract regardless of the harness's console-mode
// settings — shadow-terminal itself gates its interactive test
// off Windows for the same reason. The non-interactive PTY suite
// (srd92_display) still runs everywhere.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::SteppableTerminal;

mod pty_support;
use pty_support::wait_for;

/// Path to the `tui_display_harness` example binary. Derived from
/// the running test binary: `target/<profile>/deps/<test>` →
/// `target/<profile>/examples/tui_display_harness`.
fn harness_binary() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // drop the test binary file name
    if p.ends_with("deps") {
        p.pop(); // deps → profile dir
    }
    p.push("examples");
    p.push("tui_display_harness");
    p
}

fn harness_config() -> Config {
    let command: Vec<OsString> = vec![harness_binary().into()];
    Config {
        width: 120,
        height: 30,
        command,
        scrollback_size: 500,
        scrollback_step: 5,
    }
}

fn cmd(stepper: &SteppableTerminal, line: &str) {
    stepper.send_command(line).expect("send harness command");
}

async fn screen(stepper: &mut SteppableTerminal) -> String {
    let _ = stepper.render_all_output().await;
    stepper.screen_as_string().unwrap_or_default()
}

/// Pre-close gap: the REPL toggle debounce is 250 ms measured at
/// the harness's processing clock; waiting 2× from the *observed*
/// open keeps a close from ever being swallowed by scheduling
/// jitter between observation and processing.
const TOGGLE_GAP: Duration = Duration::from_millis(500);

/// Poll until `pred` holds AND the frame is stable (two
/// consecutive identical reads) — the settle-to-predicate
/// replacement for sleep-then-snapshot. Snapshotting after a
/// fixed sleep raced the sink's ~50 ms render cadence and the
/// PTY's partial-frame reads; converging on the *expected* state
/// keeps full regression power (a real renderer bug fails here by
/// timeout, with the last screen attached) without the timing
/// guess.
async fn settled_screen(
    stepper: &mut SteppableTerminal,
    what: &str,
    pred: impl Fn(&str) -> bool,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut prev: Option<String> = None;
    loop {
        let s = screen(stepper).await;
        if pred(&s) && prev.as_deref() == Some(s.as_str()) {
            return s;
        }
        prev = Some(s);
        if tokio::time::Instant::now() >= deadline {
            pty_support::kill_and_panic(
                stepper,
                &format!("screen never settled into expected state: {what}"),
            )
            .await;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// The blank-vs-content shape of each screen row (trailing
/// whitespace trimmed). Ignores the margin's advancing session clock
/// and any padding, so a structural assertion catches injected /
/// scrolled-in blank lines without being brittle to the wall clock.
fn blank_shape(s: &str) -> Vec<bool> {
    s.lines().map(|l| l.trim().is_empty()).collect()
}

/// Console (REPL) output is CONTAINED in the console frame: it
/// renders inside the console (on the alternate screen) and, on
/// close, the primary surface is restored without it — the output
/// never leaks onto the primary scrollback. The old behaviour
/// wrote command echo/response straight to stderr scrollback,
/// which this guards against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sysmon_detail_line_renders_under_the_active_phase() {
    // Drive the REAL sink: install a tree, start a phase, feed one sysmon
    // sample, and assert the phase block grew the utilization detail line —
    // body naming the subjects, gutter carrying glyph + braille meter.
    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");
    // An active phase with a literal render body — the harness's canonical
    // way to make the fold produce a phase block (a bare `start` carries no
    // render handle, so it folds to nothing).
    cmd(&stepper, "status alpha-live-line");
    wait_for(&mut stepper, "alpha-live-line", Duration::from_secs(5)).await;

    cmd(&stepper, "sysmon 0.97 0.34 0.89 0.41 0.93");
    let screen = settled_screen(
        &mut stepper,
        "sysmon detail line rendered",
        |s| s.contains("\u{26C3} io nvme1n1 97%"),
        Duration::from_secs(5),
    )
    .await;

    assert!(
        screen.contains("\u{2699} cpu 34% (max c7 89%)"),
        "body must name the hot core:\n{screen}"
    );
    assert!(
        screen.contains("\u{25A4} ram 41% (+cache 93%)"),
        "body must carry both memory measures:\n{screen}"
    );
    for glyph in ['⛃', '⚙', '▤'] {
        assert!(
            screen.contains(glyph),
            "gutter must carry the {glyph} item:\n{screen}"
        );
    }
    // The braille meter for 97% disk is a solid-plus cell; at minimum SOME
    // braille dots must be on screen (U+2800 block, nonzero pattern).
    assert!(
        screen
            .chars()
            .any(|c| ('\u{2801}'..='\u{28FF}').contains(&c)),
        "at least one non-empty braille meter cell must render:\n{screen}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_output_is_contained_in_frame() {
    const MARKER: &str = "CONSOLEMARKERZZ";

    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");

    // A plain scrollback log line is the stable primary surface.
    cmd(&stepper, "log baseline-anchor-line");
    let primary = settled_screen(
        &mut stepper,
        "baseline anchor rendered",
        |s| s.contains("baseline-anchor-line"),
        Duration::from_secs(5),
    )
    .await;
    assert!(!primary.contains(MARKER));

    // Open the console; emit output — it renders inside the
    // console (alternate screen).
    cmd(&stepper, "window");
    wait_for(&mut stepper, "REPL", Duration::from_secs(5)).await;
    cmd(&stepper, &format!("out {MARKER}"));
    wait_for(&mut stepper, MARKER, Duration::from_secs(5)).await;

    // Close the console: the primary surface is restored exactly,
    // and the console output is NOT on it (contained).
    tokio::time::sleep(TOGGLE_GAP).await;
    cmd(&stepper, "window");
    let after = settled_screen(
        &mut stepper,
        "primary restored without console output",
        |s| s == primary,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        !after.contains(MARKER),
        "console output must not leak onto the primary surface\n\
         --- screen ---\n{after}"
    );
    assert_eq!(
        after, primary,
        "primary surface must be restored byte-exact after console close"
    );

    let _ = stepper.kill();
}

/// Opening then closing the console must restore the primary
/// terminal surface byte-exact — no residual gap, no re-stream.
/// The console renders on the alternate screen, so the terminal
/// saves the primary on open and restores it on close (the harness
/// publishes no live status line, so the surface is otherwise
/// stable). This is the regression guard for the "console toggle
/// leaves residual state" report. Covers both the `~` bar and the
/// `` ` `` window — both live on the alternate screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_toggle_restores_surface_byte_exact() {
    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");
    for i in 0..6 {
        cmd(&stepper, &format!("log scrollback-line-{i}"));
    }
    let s0 = settled_screen(
        &mut stepper,
        "scrollback baseline rendered",
        |s| s.contains("scrollback-line-5"),
        Duration::from_secs(5),
    )
    .await;

    // --- window (`) open + close ---
    cmd(&stepper, "window");
    wait_for(&mut stepper, "REPL", Duration::from_secs(5)).await;
    tokio::time::sleep(TOGGLE_GAP).await;
    cmd(&stepper, "window");
    let after_window = settled_screen(
        &mut stepper,
        "surface restored byte-exact after window close",
        |s| s == s0,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        s0, after_window,
        "closing the window console must restore the surface byte-exact"
    );

    // --- bar (~) open + close ---
    tokio::time::sleep(TOGGLE_GAP).await;
    cmd(&stepper, "bar");
    wait_for(&mut stepper, "REPL", Duration::from_secs(5)).await;
    tokio::time::sleep(TOGGLE_GAP).await;
    cmd(&stepper, "bar");
    let after_bar = settled_screen(
        &mut stepper,
        "surface restored byte-exact after bar close",
        |s| s == s0,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        s0, after_bar,
        "closing the bar console must restore the surface byte-exact"
    );

    let _ = stepper.kill();
}

/// The live status block changes height during a run (the memo line
/// and the running-phase braille bar appear and disappear). Growing
/// then shrinking it must NOT leave a fully-blank (no-margin) line
/// wedged between the scrollback logs and the status — and it must
/// not persist when no new logs follow (a long phase that only
/// updates its status). This is the actual "injecting empty lines"
/// regression.
///
/// Fixed by the shrink-slide in `log_only_sink`: when the status
/// shrinks, the log area is slid down (reverse-index inside a scroll
/// region excluding the status rows) so the freed rows fill with real
/// log content and the unavoidable blank lands at the top of the
/// screen (leading whitespace), never as a gap directly above the
/// status / between events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_height_change_leaves_no_blank_gap() {
    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");

    // Fill the viewport so the content is contiguous (no natural top
    // gap to confuse the assertion). Let the logs fully settle before
    // touching the status, so the status-height changes are isolated
    // from the log-emit path (otherwise a log batch and a height
    // change land in the same redraw and interleave).
    for i in 0..40 {
        cmd(&stepper, &format!("log fill-line-{i:02}"));
    }
    wait_for(&mut stepper, "fill-line-39", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    cmd(&stepper, "status STAT-A| STAT-B");
    wait_for(&mut stepper, "STAT-A", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    cmd(&stepper, "status STAT-A| STAT-B| STAT-C"); // grow 2 -> 3
    wait_for(&mut stepper, "STAT-C", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    cmd(&stepper, "status STAT-A| STAT-B"); // shrink 3 -> 2
    // No new logs after the shrink — mimic a long phase that only
    // updates its status.
    let s = settled_screen(
        &mut stepper,
        "status shrink applied (STAT-C gone)",
        |s| !s.contains("STAT-C") && s.contains("STAT-B"),
        Duration::from_secs(5),
    )
    .await;

    // No fully-blank line may sit between two content lines.
    let lines: Vec<&str> = s.lines().collect();
    for w in lines.windows(3) {
        let mid_blank = w[1].trim().is_empty();
        let neighbors_content = !w[0].trim().is_empty() && !w[2].trim().is_empty();
        assert!(
            !(mid_blank && neighbors_content),
            "status height change left a blank gap between content lines:\n{s}"
        );
    }

    let _ = stepper.kill();
}

/// The user's actual regression: a log line emitted in the same redraw
/// window as a status-height change (a scope-readout landing just as
/// the phase's status block grows or shrinks). The absolute-positioned
/// renderer stranded a blank between the logs and the status AND could
/// DROP the colliding log entirely (it was written into the status
/// region and overwritten by the re-pin). Follow-the-log keeps every
/// log and leaves no gap between content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_status_collision_keeps_logs_without_gap() {
    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");

    for i in 0..6 {
        cmd(&stepper, &format!("log fill-{i:02}"));
    }
    wait_for(&mut stepper, "fill-05", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    cmd(&stepper, "status RUN-A| RUN-B");
    wait_for(&mut stepper, "RUN-A", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Collision 1: a log + a status GROW, back to back (same poll
    // window → same redraw).
    cmd(&stepper, "log EVENT-ALPHA");
    cmd(&stepper, "status RUN-A| RUN-B| MEMO");
    wait_for(&mut stepper, "MEMO", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Collision 2: a log + a status SHRINK, back to back.
    cmd(&stepper, "log EVENT-BETA");
    cmd(&stepper, "status RUN-A| RUN-B");
    let s = settled_screen(
        &mut stepper,
        "shrink-colliding log rendered (MEMO gone, EVENT-BETA in)",
        |s| s.contains("EVENT-BETA") && !s.contains("MEMO"),
        Duration::from_secs(5),
    )
    .await;

    // Both colliding logs survived (the old renderer dropped them).
    assert!(
        s.contains("EVENT-ALPHA"),
        "grow-colliding log was dropped:\n{s}"
    );
    assert!(
        s.contains("EVENT-BETA"),
        "shrink-colliding log was dropped:\n{s}"
    );
    // No fully-blank line wedged between two content lines.
    let lines: Vec<&str> = s.lines().collect();
    for w in lines.windows(3) {
        let mid_blank = w[1].trim().is_empty();
        let neighbors_content = !w[0].trim().is_empty() && !w[2].trim().is_empty();
        assert!(
            !(mid_blank && neighbors_content),
            "status/log collision left a blank gap between content:\n{s}"
        );
    }

    let _ = stepper.kill();
}

/// A multi-line log block — a phase-outcome error readout (a CQL error
/// + the offending statement + an embedded blank row + a `(+N more)`
/// tail) — emitted in the same redraw window as a status height change.
/// This is the real error-emission shape from a live run. The
/// absolute-positioned renderer wrote logs into the status region and
/// re-pinned over them, so the TAIL lines of a multi-row block could be
/// dropped; follow-the-log emits the whole block (it scrolls) and draws
/// the status beneath it. The embedded blank row carries the margin
/// (it's log content, not a stranded gap), so it is not a fully-blank
/// line between content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiline_error_block_emits_fully_with_status_change() {
    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");

    // Fill the viewport so this mirrors a live run (status pinned at
    // the bottom), not a sparse start.
    for i in 0..30 {
        cmd(&stepper, &format!("log fill-{i:02}"));
    }
    wait_for(&mut stepper, "fill-29", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    cmd(&stepper, "status RUNNING-A| RUNNING-B");
    wait_for(&mut stepper, "RUNNING-A", Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The phase-outcome error block (`||` = the embedded blank row)
    // lands together with a status grow (the running phase's memo
    // appearing) — back to back, same poll window.
    cmd(&stepper, "log ERRHEAD|ERRSTMT|ERRVALUES||ERRPLUS63");
    cmd(&stepper, "status RUNNING-A| RUNNING-B| MEMO");
    let s = settled_screen(
        &mut stepper,
        "error block + grown status rendered",
        |s| s.contains("MEMO") && s.contains("ERRPLUS63"),
        Duration::from_secs(5),
    )
    .await;

    // Every line of the block survived — including the TAIL, which the
    // old renderer could overwrite.
    for line in ["ERRHEAD", "ERRSTMT", "ERRVALUES", "ERRPLUS63"] {
        assert!(s.contains(line), "error-block line {line:?} dropped:\n{s}");
    }
    // The status block landed too.
    assert!(
        s.contains("MEMO") && s.contains("RUNNING-B"),
        "status not drawn beneath the error block:\n{s}"
    );
    // No fully-blank (no-margin) line wedged between content — the
    // embedded `||` blank carries the margin so it is NOT flagged.
    let lines: Vec<&str> = s.lines().collect();
    for w in lines.windows(3) {
        let mid_blank = w[1].trim().is_empty();
        let neighbors_content = !w[0].trim().is_empty() && !w[2].trim().is_empty();
        assert!(
            !(mid_blank && neighbors_content),
            "multi-line error block left a blank gap between content:\n{s}"
        );
    }

    let _ = stepper.kill();
}

/// Swap-consistency smoke test: a Ctrl-T cycle (sink teardown +
/// alt-screen save/restore + a fresh sink) over an unchanged snapshot
/// must re-render the same blank-line structure with the top log line
/// still visible — `display = f(snapshot)`, swap-invariant.
///
/// NOTE: this does NOT reproduce the live "blank lines accumulate per
/// Ctrl-T" report. That bug depends on the real terminal's alternate-
/// screen save/restore interacting with a *live, changing* status
/// block; shadow-terminal's simplified swap nets to a no-op here, so
/// this test passes both with and without the `first_paint` fix in
/// `log_only_sink`. Kept as a genuine swap-invariant guard; the
/// accumulation regression still needs a faithful reproduction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_re_renders_surface_byte_identically() {
    let mut stepper = SteppableTerminal::start(harness_config())
        .await
        .expect("spawn harness under shadow terminal");

    // Scrollback logs + a 2-row live status block (the `|` splits
    // rows). The 2-row status is what makes scroll-on-growth fire.
    cmd(&stepper, "log scroll-line-one");
    cmd(&stepper, "log scroll-line-two");
    cmd(&stepper, "status RUNNING phase| 120/s ok:100% c:1");
    let s0 = settled_screen(
        &mut stepper,
        "status + scrollback baseline rendered",
        |s| s.contains("RUNNING phase") && s.contains("scroll-line-two"),
        Duration::from_secs(5),
    )
    .await;
    let shape0 = blank_shape(&s0);

    assert!(
        s0.contains("scroll-line-two"),
        "top log line visible in s0: {s0}"
    );

    // Three swaps; the snapshot never changes, so each must re-render
    // the same blank-line structure with the top log line still
    // visible. Pre-fix, the fresh sink's first paint scrolled the
    // 2-row status block in, drifting content up and accumulating
    // blank rows on every swap (so `scroll-line-two` walked off the
    // top and the shape changed).
    for i in 0..3 {
        cmd(&stepper, "swap");
        let after = settled_screen(
            &mut stepper,
            &format!(
                "swap #{i} re-rendered with the baseline blank-line \
                 structure and the top log line visible"
            ),
            |s| {
                s.contains("RUNNING phase")
                    && s.contains("scroll-line-two")
                    && blank_shape(s) == shape0
            },
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            blank_shape(&after),
            shape0,
            "swap #{i} must not change the blank-line structure \
             (no injected/accumulated blanks)\n--- s0 ---\n{s0}\n\
             --- after swap #{i} ---\n{after}"
        );
        assert!(
            after.contains("scroll-line-two"),
            "swap #{i} must not scroll the top log line off the surface\n{after}"
        );
    }

    let _ = stepper.kill();
}

/// Regression for the log-line DROP bug: a burst of log lines far
/// exceeding the bounded ring capacity (200) must ALL reach the
/// scrollback — none dropped, in order — even when they arrive faster
/// than the ~50 ms render cadence can drain them.
///
/// This drives the REAL [`nmbrs_tui::log_only_sink::LogOnlySink`]
/// render loop headlessly (no PTY needed — the sink falls back to a
/// default terminal size and writes every drained line to stderr,
/// which the terminal scrolls). It pipes 300 `log` commands to the
/// harness in one shot so they all land in the actor before the first
/// drain tick — the exact overflow condition that used to print
/// `dropped N log line(s)` and lose the earliest 100 lines. The fix
/// carries scrollback on a durable, unbounded stream the sink drains
/// FULLY every tick, so stderr must contain every line and no drop
/// banner.
#[test]
fn scrollback_burst_emits_every_line_without_drop() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    const N: usize = 300; // > the 200-line ring capacity

    // Per-test sandbox cwd under target (the harness is a mock and
    // writes nothing to disk, but keep the run out of the repo root).
    let cwd = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scrollback_burst");
    let _ = std::fs::create_dir_all(&cwd);

    let mut child = Command::new(harness_binary())
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tui_display_harness");

    {
        let mut stdin = child.stdin.take().expect("harness stdin");
        // Burst all N before the sink's first ~50 ms drain tick.
        for i in 0..N {
            writeln!(stdin, "log burst-{i:04}").expect("write burst line");
        }
        stdin.flush().expect("flush burst");
        // Let the ~50 ms sink drain (many ticks' worth of headroom).
        std::thread::sleep(Duration::from_millis(700));
        writeln!(stdin, "quit").expect("write quit");
    }

    let out = child.wait_with_output().expect("await harness");
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(
        !err.contains("dropped"),
        "the durable scrollback stream must never drop a line — found a \
         drop banner in the harness output"
    );
    // Every single line reached the scrollback, in order.
    for i in 0..N {
        assert!(
            err.contains(&format!("burst-{i:04}")),
            "scrollback line burst-{i:04} was dropped (renderer lag must \
             not lose lines)"
        );
    }
}
