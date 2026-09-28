// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0
//
// These async tests serialize on a process-global `TEST_LOCK` (a pure
// `Mutex<()>`) held across `.await` so global session-root installs don't
// race; the awaited router code never locks it, so there's no deadlock —
// `await_holding_lock` is a false positive for this deliberate pattern.
#![allow(clippy::await_holding_lock)]

//! Axum integration tests for the `/api/controls` list + the
//! `/api/control/{name}` POST write endpoint (SRD 23 §"Web API").
//!
//! Tests drive the router directly via `tower::ServiceExt::oneshot`
//! — no real socket is bound. Each test installs a session root
//! with the controls the endpoint will resolve through.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use nmbrs_metrics::component::Component;
use nmbrs_metrics::controls::{BranchScope, ControlBuilder};
use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::polydat_nodes::runtime_context::set_session_root;
use nmbrs_web::ws::MetricsBroadcast;

/// Sequence tests that touch the process-global session root.
static TEST_LOCK: Mutex<()> = Mutex::new(());

async fn read_body(body: Body) -> String {
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn build_router() -> axum::Router {
    nmbrs_web::server::build_router(MetricsBroadcast::new(16))
}

fn install_rate_control(initial: f64) -> std::sync::Arc<std::sync::RwLock<Component>> {
    let root = Component::root(
        Labels::empty()
            .with("type", "session")
            .with("session", "web_api_test"),
        HashMap::new(),
    );
    root.read().unwrap().controls().declare(
        ControlBuilder::new("rate", initial)
            .reify_as_gauge(|v: &f64| Some(*v))
            .from_f64(|v| {
                if v <= 0.0 {
                    Err("rate must be positive".into())
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

#[tokio::test]
async fn list_controls_empty_when_no_controls_declared() {
    let _g = TEST_LOCK.lock().unwrap();
    // Install a session with no controls so the endpoint sees an
    // empty registry rather than picking up residue from another
    // test.
    let root = Component::root(Labels::of("session", "empty"), HashMap::new());
    set_session_root(root);

    let app = build_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/controls")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp.into_body()).await;
    assert_eq!(body.trim(), "[]");
}

#[tokio::test]
async fn list_controls_returns_every_declared_control() {
    let _g = TEST_LOCK.lock().unwrap();
    install_rate_control(100.0);

    let app = build_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/controls")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp.into_body()).await;
    // JSON includes the rate control with expected metadata.
    assert!(body.contains("\"name\":\"rate\""), "body: {body}");
    assert!(body.contains("\"accepts_f64_writes\":true"), "body: {body}");
    assert!(body.contains("\"scope\":\"subtree\""), "body: {body}");
}

#[tokio::test]
async fn set_control_writes_and_returns_rev() {
    let _g = TEST_LOCK.lock().unwrap();
    let root = install_rate_control(100.0);
    let control: nmbrs_metrics::controls::Control<f64> =
        root.read().unwrap().controls().get("rate").unwrap();

    let app = build_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/control/rate")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"value":2500.0,"source":"itest"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp.into_body()).await;
    assert!(body.contains("\"submitted_value\":2500"), "body: {body}");
    assert!(body.contains("\"committed_rev\""), "body: {body}");
    assert_eq!(control.value(), 2500.0);
    match control.get().origin {
        nmbrs_metrics::controls::ControlOrigin::Api { source } => {
            assert_eq!(source, "itest");
        }
        other => panic!("expected Api origin, got {other:?}"),
    }
}

#[tokio::test]
async fn set_control_missing_name_returns_404() {
    let _g = TEST_LOCK.lock().unwrap();
    install_rate_control(100.0);

    let app = build_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/control/nonexistent")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"value":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = read_body(resp.into_body()).await;
    assert!(body.contains("\"code\":\"not_found\""), "body: {body}");
}

#[tokio::test]
async fn set_control_validator_rejection_returns_400() {
    let _g = TEST_LOCK.lock().unwrap();
    install_rate_control(100.0);

    let app = build_router();
    // The converter rejects rate <= 0; the response surfaces
    // the error as a structured 400.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/control/rate")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"value":-1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_body(resp.into_body()).await;
    assert!(
        body.contains("\"code\":\"validation_failed\""),
        "body: {body}"
    );
    assert!(body.contains("must be positive"), "body: {body}");
}

#[tokio::test]
async fn set_control_final_scope_returns_400_with_code() {
    let _g = TEST_LOCK.lock().unwrap();
    let root = Component::root(Labels::of("session", "final_test"), HashMap::new());
    root.read().unwrap().controls().declare(
        ControlBuilder::new("rate", 100f64)
            .reify_as_gauge(|v: &f64| Some(*v))
            .from_f64(Ok)
            .final_at_scope("session_root")
            .branch_scope(BranchScope::Subtree)
            .build(),
    );
    set_session_root(root);

    let app = build_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/control/rate")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"value":500}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_body(resp.into_body()).await;
    assert!(
        body.contains("\"code\":\"final_violation\""),
        "body: {body}"
    );
    assert!(body.contains("session_root"), "body: {body}");
}
