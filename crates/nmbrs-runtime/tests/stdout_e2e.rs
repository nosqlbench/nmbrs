// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test: YAML workload → Polydat Kernel → stdout activity.
//!
//! This is the full pipeline test — the closest thing to running
//! `nmbrs run workload.yaml driver=stdout`.

use std::sync::Arc;

use nmbrs_adapter_stdout::{StdoutAdapter, StdoutConfig, StdoutFormat};
use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::activity::{Activity, ActivityConfig};
use nmbrs_runtime::adapter::DriverAdapter;
use nmbrs_runtime::opseq::{OpSequence, SequencerType};
use nmbrs_runtime::synthesis::OpBuilder;
use nmbrs_workload::parse::parse_ops;
use polydat::compile::assembly::{PolydatAssembler, WireRef};
use polydat::library::arithmetic::Mod;
use polydat::library::hash::Hash;
use polydat::library::identity::Identity;

/// Parse a workload YAML, build a Polydat Kernel from bindings that
/// cover all referenced bind points, wire up the activity, and
/// run it through the stdout adapter via the tiered DriverAdapter interface.
#[tokio::test]
async fn full_pipeline_simple_workload() {
    let yaml = r#"
bindings:
  user_id: Identity()
ops:
  write_user:
    stmt: "INSERT INTO users (id) VALUES ({user_id});"
"#;

    let ops = parse_ops(yaml).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].name, "write_user");

    let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
    asm.add_node(
        "user_id",
        Box::new(Identity::new(polydat::ast::PortType::U64)),
        vec![WireRef::input("cycle")],
    );
    asm.add_output("user_id", WireRef::node("user_id"));
    let kernel = asm.compile().unwrap();

    let builder = Arc::new(OpBuilder::new(kernel));

    let config = ActivityConfig {
        name: "test".into(),
        cycles: 5,
        concurrency: 1,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "e2e"), seq);

    let path = std::env::temp_dir().join("nb_e2e_test.txt");
    let adapter: Arc<dyn DriverAdapter> = Arc::new(StdoutAdapter::with_config(StdoutConfig {
        filename: path.to_str().unwrap().into(),
        format: StdoutFormat::Statement,
        ..Default::default()
    }));

    activity.run_with_driver(adapter, builder.clone()).await;

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 5, "should have 5 lines (5 cycles)");

    for (i, line) in lines.iter().enumerate() {
        assert!(line.contains("INSERT INTO users"), "line {i}: {line}");
        assert!(
            !line.contains("{user_id}"),
            "bind point should be resolved: {line}"
        );
        assert!(
            line.contains(&i.to_string()),
            "line {i} should contain cycle {i}: {line}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn full_pipeline_with_hash_and_mod() {
    let yaml = r#"
bindings:
  user_id: Hash(); Mod(1000000)
  bucket: Hash(); Mod(64)
ops:
  read:
    stmt: "SELECT * FROM users WHERE id={user_id} AND bucket={bucket};"
"#;

    let ops = parse_ops(yaml).unwrap();

    let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
    asm.add_node("h1", Box::new(Hash::new()), vec![WireRef::input("cycle")]);
    asm.add_node(
        "user_id",
        Box::new(Mod::new(1_000_000)),
        vec![WireRef::node("h1")],
    );
    asm.add_node("h2", Box::new(Hash::new()), vec![WireRef::node("h1")]);
    asm.add_node("bucket", Box::new(Mod::new(64)), vec![WireRef::node("h2")]);
    asm.add_output("user_id", WireRef::node("user_id"));
    asm.add_output("bucket", WireRef::node("bucket"));
    let kernel = asm.compile().unwrap();
    let builder = Arc::new(OpBuilder::new(kernel));

    let config = ActivityConfig {
        name: "readtest".into(),
        cycles: 10,
        concurrency: 2,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "e2e"), seq);

    let path = std::env::temp_dir().join("nb_e2e_hash_test.txt");
    let adapter: Arc<dyn DriverAdapter> = Arc::new(StdoutAdapter::with_config(StdoutConfig {
        filename: path.to_str().unwrap().into(),
        format: StdoutFormat::Statement,
        ..Default::default()
    }));

    activity.run_with_driver(adapter, builder.clone()).await;

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 10);

    for line in &lines {
        assert!(line.contains("SELECT * FROM users WHERE id="));
        assert!(line.contains("AND bucket="));
        assert!(!line.contains("{user_id}"));
        assert!(!line.contains("{bucket}"));
    }

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn full_pipeline_weighted_ops() {
    let yaml = r#"
ops:
  read:
    ratio: 3
    stmt: "READ"
  write:
    ratio: 1
    stmt: "WRITE"
"#;

    let ops = parse_ops(yaml).unwrap();

    let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
    asm.add_node(
        "id",
        Box::new(Identity::new(polydat::ast::PortType::U64)),
        vec![WireRef::input("cycle")],
    );
    asm.add_output("id", WireRef::node("id"));
    let kernel = asm.compile().unwrap();
    let builder = Arc::new(OpBuilder::new(kernel));

    let config = ActivityConfig {
        name: "weighted".into(),
        cycles: 8,
        concurrency: 1,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    assert_eq!(seq.stanza_length(), 4);

    let activity = Activity::new(config, &Labels::of("session", "e2e"), seq);

    let path = std::env::temp_dir().join("nb_e2e_weighted_test.txt");
    let adapter: Arc<dyn DriverAdapter> = Arc::new(StdoutAdapter::with_config(StdoutConfig {
        filename: path.to_str().unwrap().into(),
        format: StdoutFormat::Statement,
        ..Default::default()
    }));

    activity.run_with_driver(adapter, builder.clone()).await;

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 8);

    let reads = lines.iter().filter(|l| l.contains("READ")).count();
    let writes = lines.iter().filter(|l| l.contains("WRITE")).count();
    assert_eq!(reads, 6, "3:1 ratio over 8 cycles = 6 reads");
    assert_eq!(writes, 2, "3:1 ratio over 8 cycles = 2 writes");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn full_pipeline_json_format() {
    let yaml = r#"
ops:
  create:
    method: POST
    url: "/api/users/{user_id}"
    content_type: "application/json"
"#;

    let ops = parse_ops(yaml).unwrap();

    let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
    asm.add_node("h", Box::new(Hash::new()), vec![WireRef::input("cycle")]);
    asm.add_node(
        "user_id",
        Box::new(Mod::new(10000)),
        vec![WireRef::node("h")],
    );
    asm.add_output("user_id", WireRef::node("user_id"));
    let kernel = asm.compile().unwrap();
    let builder = Arc::new(OpBuilder::new(kernel));

    let config = ActivityConfig {
        name: "jsontest".into(),
        cycles: 3,
        concurrency: 1,
        ..Default::default()
    };
    let seq = OpSequence::from_ops(ops, SequencerType::Bucket);
    let activity = Activity::new(config, &Labels::of("session", "e2e"), seq);

    let path = std::env::temp_dir().join("nb_e2e_json_test.txt");
    let adapter: Arc<dyn DriverAdapter> = Arc::new(StdoutAdapter::with_config(StdoutConfig {
        filename: path.to_str().unwrap().into(),
        format: StdoutFormat::Json,
        ..Default::default()
    }));

    activity.run_with_driver(adapter, builder.clone()).await;

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 3);

    for line in &lines {
        assert!(line.starts_with('{'), "should be JSON: {line}");
        assert!(
            line.contains("\"method\":\"POST\""),
            "should have method: {line}"
        );
        assert!(
            line.contains("/api/users/"),
            "should have url with resolved id: {line}"
        );
        assert!(
            !line.contains("{user_id}"),
            "bind point should be resolved: {line}"
        );
    }

    let _ = std::fs::remove_file(&path);
}
