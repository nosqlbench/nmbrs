// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Bangpath (shebang) invocation — a workload whose first line is
//! `#!/usr/bin/env nmbrs` is directly executable by unix shells.
//!
//! The kernel turns `./wl.yaml smoke k=v` into
//! `/usr/bin/env nmbrs /abs/path/wl.yaml smoke k=v`, so nmbrs sees the
//! script path as argv[1] and the script's own arguments after it.
//! That is exactly the bare-workload-file shortcut: the path maps to
//! `workload=<path>`, the first bare word to `scenario=<word>`, and
//! every `k=v` / value-flag passes through as an ordinary override.
//!
//! Covered here:
//! - the bare dispatch form (`nmbrs wl.yaml …`) with scenario and
//!   param overrides taking effect,
//! - TRUE shebang execution (the script itself is the program, nmbrs
//!   found via PATH),
//! - `nmbrs copy` of a shebang'd bundled workload keeping the
//!   interpreter line at byte 0 (above the provenance stamp) and
//!   stamping exec bits, so materialized copies stay executable.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nmbrs-bangpath-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A minimal two-scenario workload: proving the positional word maps
/// to `scenario=` needs the non-default scenario to produce output
/// the default one cannot, and proving `k=v` overrides land needs a
/// param-driven extent.
const SMOKE_WORKLOAD: &str = r#"#!/usr/bin/env nmbrs
description: bangpath invocation smoke workload

params:
  beta_cycles: "2"

scenarios:
  default:
    - alpha
  smoke:
    - beta

phases:
  alpha:
    adapter: stdout
    cycles: 2
    concurrency: 1
    bindings: |
      n := cycle
    ops:
      a:
        stmt: "alpha-mark {n}"
  beta:
    adapter: stdout
    cycles: "{beta_cycles}"
    concurrency: 1
    bindings: |
      n := cycle
    ops:
      b:
        stmt: "beta-mark {n}"
"#;

fn write_workload(sandbox: &Sandbox) -> PathBuf {
    let wl = sandbox.path().join("smoke.yaml");
    std::fs::write(&wl, SMOKE_WORKLOAD).expect("write workload");
    wl
}

fn run_captured(mut cmd: Command, sandbox: &Sandbox) -> (String, String, bool) {
    let session = sandbox.path().join("session");
    cmd.current_dir(sandbox.path())
        .arg("smoke")
        .arg("beta_cycles=3")
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    // ETXTBSY (raw 26) retry: exec'ing a just-written script can race
    // a concurrently forked sibling test process that momentarily
    // holds the inherited write fd open. Bounded retry is the
    // standard remedy (cargo does the same for its own test binaries).
    let mut tries = 0;
    let out = loop {
        match cmd.output() {
            Ok(out) => break out,
            Err(e) if e.raw_os_error() == Some(26) && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("spawn: {e}"),
        }
    };
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn assert_overrides_took_effect(stdout: &str, stderr: &str, ok: bool, form: &str) {
    assert!(ok, "{form}: run must succeed:\n{stdout}\n{stderr}");
    let beta_marks = stdout.matches("beta-mark").count();
    assert_eq!(
        beta_marks, 3,
        "{form}: beta_cycles=3 argv override must set the extent:\n{stdout}\n{stderr}"
    );
    assert!(
        !stdout.contains("alpha-mark"),
        "{form}: positional `smoke` must select the beta scenario, \
         not run the default:\n{stdout}\n{stderr}"
    );
}

/// `nmbrs wl.yaml smoke beta_cycles=3 …` — the dispatch path shebang
/// execution rides on, exercised directly.
#[test]
fn bare_file_dispatch_maps_scenario_and_overrides() {
    let sb = Sandbox::new("bare");
    let wl = write_workload(&sb);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.arg(&wl);
    let (stdout, stderr, ok) = run_captured(cmd, &sb);
    assert_overrides_took_effect(&stdout, &stderr, ok, "bare dispatch");
}

/// TRUE shebang execution: the workload file IS the program. The
/// kernel hands the script path to `/usr/bin/env nmbrs`, which must
/// find nmbrs on PATH — prefixed here with the test binary's dir.
#[test]
#[cfg(unix)]
fn shebang_execution_runs_the_workload_directly() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new("shebang");
    let wl = write_workload(&sb);
    let mut perms = std::fs::metadata(&wl).unwrap().permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(&wl, perms).expect("chmod +x");

    let nmbrs_bin = Path::new(env!("CARGO_BIN_EXE_nmbrs"));
    let bin_dir = nmbrs_bin.parent().expect("binary dir");
    let path_var = std::env::var("PATH").unwrap_or_default();

    let mut cmd = Command::new(&wl);
    cmd.env("PATH", format!("{}:{path_var}", bin_dir.display()));
    let (stdout, stderr, ok) = run_captured(cmd, &sb);
    assert_overrides_took_effect(&stdout, &stderr, ok, "shebang exec");
}

/// `nmbrs copy` of a shebang'd bundled workload: the interpreter line
/// stays at byte 0 (the provenance stamp goes under it) and the copy
/// carries exec bits — materialization preserves direct executability.
#[test]
#[cfg(unix)]
fn copy_keeps_shebang_first_and_exec_bits() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new("copy");
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(sb.path())
        .args(["copy", "cql/vector_suite/vector_suite_cql_oss_sift128"])
        .output()
        .expect("spawn copy");
    assert!(
        out.status.success(),
        "copy must succeed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let dest = sb
        .path()
        .join("cql_vector_suite_vector_suite_cql_oss_sift128.yaml");
    let text = std::fs::read_to_string(&dest).expect("read copy");
    assert!(
        text.starts_with("#!/usr/bin/env nmbrs\n"),
        "shebang must stay on line 1:\n{}",
        text.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("# Copied from bundled workload")),
        "provenance stamp goes directly under the shebang:\n{}",
        text.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    let mode = std::fs::metadata(&dest)
        .expect("stat copy")
        .permissions()
        .mode();
    assert!(
        mode & 0o100 != 0,
        "materialized copy of a shebang'd workload must be executable, mode={mode:o}"
    );
}
