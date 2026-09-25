// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-88 — the piece that lets the example VERIFIER run in-process: each
//! concurrent execution's op STDOUT is captured **per-execution** (its own
//! `CaptureChannel`, scoped via `ExecutionContext`), not collided on the one
//! process fd. With that, the verifier can check `expect` / `expect-fail`
//! regexes against each execution's output — the SAME rule-checking
//! (`nmbrs_workload::verify::check_case_output`) the subprocess path uses —
//! while N examples run concurrently sharing ONE session.
//!
//! In-process, so the stdout adapter is force-linked.

extern crate nmbrs_adapter_stdout;

use std::path::PathBuf;
use std::sync::Arc;

use nmbrs_runtime::concurrent::HeadlessObserver;
use nmbrs_runtime::observer::RunObserver;
use nmbrs_runtime::output_channel::{CaptureChannel, OutputChannel};
use nmbrs_runtime::runner::{ExecutionSpec, run_executions};
use nmbrs_workload::verify::{VerifyCase, check_case_output};

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
            std::env::temp_dir().join(format!("nmbrs-verify-ip-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_executions_capture_their_own_output_and_pass_their_checks() {
    let tmp = TempDir::new();
    let session = tmp.path.join("s");
    let session_args: Vec<String> = vec![
        "--session-path".into(),
        session.to_string_lossy().into_owned(),
    ];

    // Three executions, each emitting a DISTINCT op-output marker. Each gets its
    // own CaptureChannel — kept here so we can read what it captured.
    let n = 3usize;
    let captures: Vec<Arc<CaptureChannel>> =
        (0..n).map(|_| Arc::new(CaptureChannel::new())).collect();
    let specs: Vec<ExecutionSpec> = (0..n)
        .map(|i| ExecutionSpec {
            args: vec![
                format!("op=ex{i}-{{cycle}}"),
                "cycles=3".into(),
                "adapter=stdout".into(),
            ],
            observer: Arc::new(HeadlessObserver::new()) as Arc<dyn RunObserver>,
            channel: Some(captures[i].clone() as Arc<dyn OutputChannel>),
        })
        .collect();

    let results = run_executions(&session_args, Arc::new(HeadlessObserver::new()), specs, 3)
        .await
        .expect("run_executions: session setup");
    assert_eq!(results.len(), n);

    for i in 0..n {
        assert!(
            results[i].is_ok(),
            "execution {i} errored: {:?}",
            results[i]
        );
        let combined = captures[i].op_lines().join("\n");

        // Per-execution capture isolation: execution i captured ITS OWN op
        // output (`exi-…`) and NONE of its siblings' — proving op stdout routes
        // to the execution's own channel, not the shared process fd.
        assert!(
            combined.contains(&format!("ex{i}-0")),
            "execution {i} should capture its own op output; got: {combined:?}"
        );
        for j in 0..n {
            if j != i {
                assert!(
                    !combined.contains(&format!("ex{j}-")),
                    "execution {i} must NOT capture sibling ex{j}'s output: {combined:?}"
                );
            }
        }

        // And the verifier's OWN rule-checking passes on that captured output —
        // the same `check_case_output` the subprocess walker uses, now fed an
        // in-process execution's captured stdout.
        let case = VerifyCase {
            again: Vec::new(),
            session_cwd: false,
            name: format!("ex{i}"),
            run_args: vec![],
            expects: vec![regex::Regex::new(&format!("ex{i}-1")).unwrap()],
            expect_fails: vec![],
            timeout: 30,
        };
        check_case_output(&case, &combined, results[i].is_ok(), false)
            .unwrap_or_else(|e| panic!("verify check failed for execution {i}: {e}"));
    }
}

mod scratch;
