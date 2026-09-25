// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Plotter rendering-alignment tests via the shadow-terminal harness.
//!
//! [`shadow_terminal::SteppableTerminal`] gives an in-memory, fully
//! crossterm-rendered terminal of a fixed size. We run a plotter
//! workload inside one and read back the rendered cells, then assert the
//! plot stays inside the canvas — every row fits the width (nothing
//! wraps), the title and the run-summary both survive (so the plot
//! didn't overflow its vertical budget and stagger the layout), and the
//! distribution actually drew braille content.
//!
//! `render=single` exercises the scrollback print path directly (the one
//! that emits one row per `println!`, where a full-width line could trip
//! the terminal's last-column auto-wrap). The plotter reserves the last
//! column when it sizes itself off the terminal — via
//! `crossterm::terminal::size()`, the same call shadow-terminal honours —
//! so no row should ever reach the edge. The plotter `height=` is set
//! smaller than the terminal so the whole image (title + plot + summary)
//! fits on one screen; the WIDTH is left to auto-detection so the
//! last-column reserve is the thing under test.
//!
//! The workloads are written into a tempdir (the corrected distribution
//! bindings — `unit_interval`-bridged hashes) so the test is
//! self-contained and the plotter params land in the op's `params:`
//! surface the adapter actually reads.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::SteppableTerminal;

mod pty_support;
use pty_support::{settle, wait_for};

fn nmbrs_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nmbrs"))
}

/// Hand-rolled tempdir for the workload + `--session-path`, cleaned on drop.
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
            std::env::temp_dir().join(format!("nmbrs-plot-align-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Write a plotter workload with the three corrected distributions.
/// `extra_params` are extra `params:` lines (e.g. `mode: histogram`).
/// `plot_h` caps the plot height so the whole image fits the screen.
fn write_workload(dir: &Path, name: &str, plot_h: u16, extra_params: &str) -> PathBuf {
    let path = dir.join(format!("{name}.yaml"));
    let yaml = format!(
        "params:\n\
        \x20 adapter: plotter\n\
        \x20 cycles: \"2500\"\n\
        \x20 render: single\n\
        \x20 height: \"{plot_h}\"\n\
        {extra_params}\
        bindings: |\n\
        \x20 input cycle: u64\n\
        \x20 uniform     := unit_interval(hash(cycle))\n\
        \x20 normal      := dist_normal(unit_interval(hash(hash(cycle))), 0.5, 0.15)\n\
        \x20 exponential := dist_exponential(unit_interval(hash(hash(hash(cycle)))), 2.0)\n\
        ops:\n\
        \x20 sample:\n\
        \x20\x20\x20 uniform: \"{{uniform}}\"\n\
        \x20\x20\x20 normal: \"{{normal}}\"\n\
        \x20\x20\x20 exponential: \"{{exponential}}\"\n",
    );
    std::fs::write(&path, yaml).expect("write workload");
    path
}

/// Like `write_workload` but LIVE (alt-screen) — no `render=single` — so the
/// plotter animates on the alt-screen and, at shutdown, leaves it and prints
/// the static plot to scrollback. This exercises the alt-screen + shutdown
/// path that `render=single` skips, where the plot's multi-line output can
/// race the run's shutdown diagnostics.
fn write_live_workload(dir: &Path, name: &str, plot_h: u16, extra_params: &str) -> PathBuf {
    let path = dir.join(format!("{name}.yaml"));
    let yaml = format!(
        "params:\n\
        \x20 adapter: plotter\n\
        \x20 cycles: \"2500\"\n\
        \x20 height: \"{plot_h}\"\n\
        {extra_params}\
        bindings: |\n\
        \x20 input cycle: u64\n\
        \x20 uniform     := unit_interval(hash(cycle))\n\
        \x20 normal      := dist_normal(unit_interval(hash(hash(cycle))), 0.5, 0.15)\n\
        \x20 exponential := dist_exponential(unit_interval(hash(hash(hash(cycle)))), 2.0)\n\
        ops:\n\
        \x20 sample:\n\
        \x20\x20\x20 uniform: \"{{uniform}}\"\n\
        \x20\x20\x20 normal: \"{{normal}}\"\n\
        \x20\x20\x20 exponential: \"{{exponential}}\"\n",
    );
    std::fs::write(&path, yaml).expect("write workload");
    path
}

fn plot_config(workload: &Path, sessions: &Path, width: u16, height: u16) -> Config {
    let command: Vec<OsString> = vec![
        nmbrs_binary().into(),
        "run".into(),
        format!("workload={}", workload.display()).into(),
        "--session-path".into(),
        sessions.into(),
    ];
    Config {
        width,
        height,
        command,
        scrollback_size: 500,
        scrollback_step: 5,
    }
}

fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// A line that carries BOTH braille and run-diagnostic text is the
/// interleaving signature: the plot's scrollback (render thread) collided
/// with the run's log/summary (main thread) on the terminal.
fn looks_like_diagnostic(line: &str) -> bool {
    const MARKERS: &[&str] = &[
        "│",
        "phases:",
        "logs:",
        "session:",
        "metrics:",
        "[ok]",
        "completed",
        "failed",
        "shutting",
        "consolidat",
        "done.",
        "warning",
    ];
    MARKERS.iter().any(|m| line.contains(m))
}

// IGNORED (SRD-87 push 3): these were written for the old *live-render*
// plotter. The plotter now *draws once* at shutdown (SRD-87 push 1's
// single-writer rewrite), writing one frame then exiting immediately;
// `shadow-terminal` stops draining the PTY once the child exits, so it can't
// reliably capture a frame written at exit (hence the hangs). The plotter's
// TTY rendering is verified functionally (clean exit + braille under a
// draining PTY), and the console-owning context is pinned by
// `nmbrs/tests/output_channel_harness.rs`. Re-enable by reworking onto a
// continuously-draining raw-PTY harness.
#[tokio::test]
#[ignore = "obsolete for the draw-once plotter; needs a draining raw-PTY harness — see output_channel_harness"]
async fn default_draw_renders_cleanly_on_a_tty() {
    // The DEFAULT path on a TTY (no `render=`): the plot is drawn to the
    // screen exactly once, at shutdown, in place. nmbrs has raw mode on for
    // the run, so this in-emulator run reproduces (and guards) the two bugs a
    // real terminal hits:
    //   1. interleave — the plot's output (render thread) racing the run's
    //      shutdown log/summary (main thread). Serialized by `shutdown()`.
    //   2. stagger — in raw mode a bare '\n' is line-feed only (no carriage
    //      return), stair-stepping every row to the right. Fixed by CR+LF.
    let dir = TempDir::new();
    let plot_h = 22u16;
    let wl = write_live_workload(&dir.path, "hdef", plot_h, "  mode: histogram\n");
    let sessions = dir.path.join("sessions");
    let (w, h) = (110u16, 44u16);
    let mut stepper = SteppableTerminal::start(plot_config(&wl, &sessions, w, h))
        .await
        .expect("start shadow terminal");

    // The title prints when the single final frame is drawn at shutdown, so
    // waiting for it means the full shutdown sequence ran.
    wait_for(&mut stepper, "histogram:", Duration::from_secs(25)).await;
    settle(&mut stepper).await;
    let screen = stepper.screen_as_string().expect("screen");
    let lines: Vec<&str> = screen.lines().collect();

    assert!(
        screen.chars().any(is_braille),
        "no braille rendered:\n{screen}"
    );

    // (1) No line mixes braille with diagnostic text (shutdown-race interleave).
    for (i, line) in lines.iter().enumerate() {
        let braille = line.chars().filter(|&c| is_braille(c)).count();
        assert!(
            !(braille > 0 && looks_like_diagnostic(line)),
            "line {i} interleaves plot braille ({braille} cells) with diagnostic text \
             — shutdown race:\n  {line:?}\n--- screen ---\n{screen}",
        );
    }

    // (2) The title starts at column 0 — a raw-mode '\n'-only stagger pushes
    //     it (and every following row) rightward.
    let title = lines
        .iter()
        .find(|l| l.contains("histogram:"))
        .expect("title on screen");
    let title_lead = title.chars().take_while(|&c| c == ' ').count();
    assert!(
        title_lead <= 2,
        "title indented {title_lead} cols — raw-mode CR/LF stagger:\n{screen}",
    );
    // …and at least one plot row reaches the left edge: rows print from
    //   column 0, but a stagger marches every one of them right.
    let reaches_left = lines.iter().any(|l| {
        l.chars().any(is_braille) && l.chars().take_while(|&c| !is_braille(c)).count() <= 2
    });
    assert!(
        reaches_left,
        "no plot row reaches the left edge — rows are staggered:\n{screen}"
    );
}

/// Shared assertions: the title rendered intact and exactly once, braille
/// content drew, no grid row exceeds the width, and — the real
/// stagger check — the braille rows below the title fit the plot's row
/// budget. A line that overran the canvas would wrap onto a second grid
/// row, roughly doubling the braille-row count past `plot_h`.
fn assert_plot_aligned(screen: &str, title_needle: &str, width: u16, plot_h: u16) {
    let lines: Vec<&str> = screen.lines().collect();

    // No grid row exceeds the canvas width.
    for (i, l) in lines.iter().enumerate() {
        let cols = l.chars().count();
        assert!(
            cols <= width as usize,
            "line {i} is {cols} cols, exceeds terminal width {width}: {l:?}",
        );
    }

    // The title rendered intact, exactly once (a wrap would split it).
    let n_title = lines.iter().filter(|l| l.contains(title_needle)).count();
    assert_eq!(
        n_title, 1,
        "title {title_needle:?} should render once:\n{screen}"
    );
    let title_idx = lines.iter().position(|l| l.contains(title_needle)).unwrap();

    // Braille content actually drew.
    assert!(
        screen.chars().any(is_braille),
        "no braille rendered (empty plot):\n{screen}",
    );

    // The braille rows sit below the title and stay within the plot's row
    // budget — the wrap/stagger detector.
    let braille_after = lines[title_idx + 1..]
        .iter()
        .filter(|l| l.chars().any(is_braille))
        .count();
    assert!(
        braille_after > 0,
        "no braille rows below the title:\n{screen}"
    );
    assert!(
        braille_after <= plot_h as usize + 1,
        "{braille_after} braille rows below the title exceeds budget {plot_h} (+1) — wrap/stagger:\n{screen}",
    );
}

#[tokio::test]
#[ignore = "obsolete for the draw-once plotter; needs a draining raw-PTY harness — see output_channel_harness"]
async fn histogram_renders_aligned_within_canvas() {
    let dir = TempDir::new();
    let plot_h = 18u16;
    let wl = write_workload(&dir.path, "hist", plot_h, "  mode: histogram\n");
    let sessions = dir.path.join("sessions");
    let (w, h) = (100u16, 44u16);
    let mut stepper = SteppableTerminal::start(plot_config(&wl, &sessions, w, h))
        .await
        .expect("start shadow terminal");

    wait_for(&mut stepper, "histogram:", Duration::from_secs(25)).await;
    settle(&mut stepper).await;
    let screen = stepper.screen_as_string().expect("screen");
    assert_plot_aligned(&screen, "histogram:", w, plot_h);

    // Histogram-shape sanity: the baseline (bottom) braille row is at
    // least as wide as the top row — bars rise from the floor.
    let braille_rows: Vec<usize> = screen
        .lines()
        .map(|l| l.chars().filter(|&c| is_braille(c)).count())
        .filter(|&n| n > 0)
        .collect();
    if braille_rows.len() >= 4 {
        let (top, bottom) = (braille_rows[0], braille_rows[braille_rows.len() - 1]);
        assert!(
            bottom >= top,
            "histogram baseline ({bottom}) should be >= top row ({top}):\n{screen}",
        );
    }
}

#[tokio::test]
#[ignore = "obsolete for the draw-once plotter; needs a draining raw-PTY harness — see output_channel_harness"]
async fn scatter_three_lanes_render_aligned() {
    let dir = TempDir::new();
    let plot_h = 24u16;
    let wl = write_workload(&dir.path, "scatter", plot_h, "");
    let sessions = dir.path.join("sessions");
    let (w, h) = (120u16, 40u16);
    let mut stepper = SteppableTerminal::start(plot_config(&wl, &sessions, w, h))
        .await
        .expect("start shadow terminal");

    wait_for(&mut stepper, "plot: ", Duration::from_secs(25)).await;
    settle(&mut stepper).await;
    let screen = stepper.screen_as_string().expect("screen");
    assert_plot_aligned(&screen, "plot: ", w, plot_h);
}

mod scratch;
