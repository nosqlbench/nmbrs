// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-109 e2e — web-only client drivers on the http module.
//!
//! Contracts:
//! - a blueprint bound to an http implementation runs against a
//!   live endpoint: the `connect` phase's login captures the
//!   session token into a shared wire, later ops present it in a
//!   header (the mock REJECTS unauthenticated search, so recall
//!   can only compute if the auth flow worked), and the probe's
//!   `results:` projection feeds relevancy from vendor JSON;
//! - `driver=vendorx` resolves the bundled manifest end-to-end:
//!   library pulled as the workload, blueprint bound, all phases
//!   traversed (dryrun — no dataset/server dependency).
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

const BLUEPRINT: &str = r#"
params:
  base_url: "http://127.0.0.1:0"
phases:
  connect:
    cycles: 1
    concurrency: 1
    ops:
      establish:
        abstract: {}
  probe:
    cycles: 2
    concurrency: 1
    bindings: |
      ground_truth := "7,3"
    ops:
      search:
        abstract:
          results:
            keys: vec_i64
        evaluations:
          relevancy:
            actual: keys
            expected: ground_truth
            k: 2
            r: 2
            functions:
              - recall
"#;

const IMPL: &str = r#"
implements: ./blueprint.yaml
bindings: |
  shared auth_token := "anonymous"
phases:
  connect:
    ops:
      establish:
        method: POST
        uri: "{base_url}/auth/login"
        body: '{"api_key": "k"}'
        capture:
          auth_token: /token
  probe:
    ops:
      search:
        method: POST
        uri: "{base_url}/search"
        headers: "api-key: {auth_token}"
        body: '{"limit": 2}'
        result:
          keys: result[*].id
"#;

/// Minimal blocking HTTP endpoint speaking the fixture dialect.
/// Runs on its own thread; each connection handles ONE request
/// (`Connection: close`). Returns the bound port.
fn spawn_mock_endpoint() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock endpoint");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until the full head + declared body arrive.
                loop {
                    let Ok(n) = sock.read(&mut chunk) else { return };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let content_len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= head_end + 4 + content_len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("/");
                let authed = text.lines().any(|l| l == "api-key: tok-123");
                let (status, body) = match path {
                    "/auth/login" => ("200 OK", r#"{"token":"tok-123"}"#),
                    "/search" if authed => ("200 OK", r#"{"result":[{"id":7},{"id":3}]}"#),
                    "/search" => ("401 Unauthorized", r#"{"error":"no api-key"}"#),
                    _ => ("404 Not Found", r#"{"error":"unknown path"}"#),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = sock.write_all(response.as_bytes());
            });
        }
    });
    port
}

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("nmbrs-webdrv-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> (String, String, bool) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
        cmd.current_dir(&self.dir)
            .arg("run")
            .arg("tui=off")
            .arg("--session-path")
            .arg(self.dir.join("session"));
        for a in args {
            cmd.arg(a);
        }
        let out = cmd.output().expect("run nmbrs");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The full SRD-109 chain against a live endpoint: login capture
/// → token header → authorized search → `result[*].id` projection
/// → recall. The mock rejects unauthenticated search, so a 100%
/// recall proves the auth token flowed through the shared wire.
#[test]
fn auth_flow_and_results_projection_against_live_endpoint() {
    let port = spawn_mock_endpoint();
    let sandbox = Sandbox::new("live");
    std::fs::write(sandbox.dir.join("blueprint.yaml"), BLUEPRINT).expect("write blueprint");
    std::fs::write(sandbox.dir.join("impl.yaml"), IMPL).expect("write impl");
    let (stdout, stderr, ok) = sandbox.run(&[
        "workload=impl.yaml",
        "adapter=http",
        &format!("base_url=http://127.0.0.1:{port}"),
    ]);
    assert!(
        ok,
        "bound run must complete; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let all = format!("{stdout}\n{stderr}");
    assert!(
        all.contains("2 completed, 0 failed"),
        "both phases complete; output:\n{all}"
    );
    // The relevancy summary goes to session.log.
    let log =
        std::fs::read_to_string(sandbox.dir.join("session/session.log")).expect("read session.log");
    assert!(
        log.contains("recall: mean=100.00%"),
        "projection feeds relevancy at 100% recall; session.log:\n{log}"
    );
}

/// `driver=vendorx` resolves the BUNDLED manifest: adapter=http,
/// library pulled as the workload, blueprint bound, all twelve
/// phases traversed in dryrun (no dataset or server needed).
#[test]
fn bundled_vendorx_driver_binds_and_traverses() {
    let sandbox = Sandbox::new("vendorx");
    let (stdout, stderr, ok) =
        sandbox.run(&["driver=vendorx", "scenario=search_perf", "dryrun=phases"]);
    assert!(
        ok,
        "dryrun must complete; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let all = format!("{stdout}\n{stderr}");
    assert!(
        all.contains("into blueprint 'vector_suite_blueprint'"),
        "the driver's library binds into the blueprint; output:\n{all}"
    );
    assert!(
        all.contains("12 completed, 0 failed"),
        "all blueprint phases traverse; output:\n{all}"
    );
}

/// A wrong token fails the probe with the endpoint's 401 —
/// pinning that the mock actually enforces auth (the positive
/// test can't pass vacuously). (A MISSED capture is a different
/// failure: the slot reads `Value::None` and the header
/// substitution raises a named BindError before any request —
/// also a hard stop, but not this test's subject.)
#[test]
fn missing_auth_flow_is_rejected_by_the_endpoint() {
    let port = spawn_mock_endpoint();
    let sandbox = Sandbox::new("noauth");
    std::fs::write(sandbox.dir.join("blueprint.yaml"), BLUEPRINT).expect("write blueprint");
    // Same implementation, but the search presents a wrong
    // literal token instead of the captured one.
    let broken = IMPL.replace("api-key: {auth_token}", "api-key: not-the-token");
    std::fs::write(sandbox.dir.join("impl.yaml"), broken).expect("write impl");
    let (stdout, stderr, ok) = sandbox.run(&[
        "workload=impl.yaml",
        "adapter=http",
        &format!("base_url=http://127.0.0.1:{port}"),
    ]);
    let all = format!("{stdout}\n{stderr}");
    assert!(
        !ok,
        "unauthenticated search must fail the run; output:\n{all}"
    );
    assert!(
        all.contains("HttpStatus401"),
        "the endpoint's 401 names the failure; output:\n{all}"
    );
}
