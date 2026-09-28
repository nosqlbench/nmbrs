//! SRD-63 op-leaf display E2E — the `LogOnlySink` managed-TUI surface.
//!
//! An op that declares `readout: visible` must surface as an indented,
//! timed status leaf nested under its (running) parent phase in the live
//! footer — the same surface `./runtest` shows. This drives the real
//! `nmbrs run` through a `shadow_terminal` PTY (so the rendered cells are
//! exactly what an operator sees, ANSI and all) and asserts the leaf lands
//! on screen.
//!
//! Uses the `testkit` adapter (SRD-41 `Auto` console preference) so
//! `tui=terminal` is honored and the managed region renders to the PTY; a
//! `result-latency` keeps the phase alive long enough to observe the leaf.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::SteppableTerminal;

mod pty_support;

fn nmbrs_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nmbrs"))
}

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
            std::env::temp_dir().join(format!("nmbrs-op-leaf-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
    fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A single long-running phase `build` whose op `flush` opts into
/// `readout: visible`. 40 cycles × 250 ms ≈ 10 s keeps `build` active
/// (and its op leaf present) well beyond the screen-poll cadence.
fn op_leaf_workload() -> (TempDir, PathBuf) {
    let dir = TempDir::new();
    let yaml_path = dir.path().join("op_leaf.yaml");
    std::fs::write(
        &yaml_path,
        r#"scenarios:
  default: [build]

phases:
  build:
    adapter: testkit
    cycles: 40
    concurrency: 1
    ops:
      flush:
        stmt: "ping"
        result-latency: "250ms"
        readout: visible
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
    assert_screen(stepper, &format!("substring {needle:?}"), timeout, |s| {
        s.contains(needle)
    })
    .await;
}

/// Poll the rendered screen until `pred` holds, or kill the child
/// and panic with a dump (shared drive loop — see `pty_support`).
async fn assert_screen(
    stepper: &mut SteppableTerminal,
    what: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) {
    pty_support::wait_until(stepper, pred, timeout, what).await;
}

/// Is there a line whose gutter is the agreed `[n/total] … │` shape — a
/// bracketed phase counter appearing before the `│` divider on the same line?
fn has_agreed_gutter(screen: &str) -> bool {
    screen.lines().any(|line| {
        let Some(bar) = line.find('│') else {
            return false;
        };
        let head = &line[..bar];
        // `[n/total]` bracketed counter somewhere left of the divider.
        head.contains('[') && head.contains('/') && head.contains(']')
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readout_visible_op_renders_as_footer_leaf() {
    let (dir, yaml) = op_leaf_workload();
    let session = dir.path().join("session");
    let config = build_config(&yaml, &session);

    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");

    // Sanity: the phase itself is rendering in the footer.
    assert_screen_contains(&mut stepper, "build", Duration::from_secs(10)).await;

    // The agreed gutter: a `[n/total] … │` counter-before-divider line.
    assert_screen(
        &mut stepper,
        "agreed [n/total] │ gutter",
        Duration::from_secs(10),
        has_agreed_gutter,
    )
    .await;

    // The op leaf: `readout: visible` op `flush` nested under `build`.
    assert_screen_contains(&mut stepper, "flush", Duration::from_secs(15)).await;

    let _ = stepper.kill();
}

mod scratch;
