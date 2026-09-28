// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Web server setup and configuration.

use axum::http::Request;
use axum::middleware;
use axum::response::Response;
use axum::{Router, routing::get, routing::post};
use tower_http::compression::CompressionLayer;

use crate::routes;
use crate::ws::{self, MetricsBroadcast};

/// Build the Axum router with all routes.
///
/// The `broadcast` parameter provides the shared metric stream.
/// In standalone mode (`nmbrs web`), create one with
/// `MetricsBroadcast::new(16)` — it will simply have no publishers.
/// In embedded mode (`nmbrs run --web`), register a
/// `broadcast.reporter()` with the metrics scheduler first.
pub fn build_router(broadcast: MetricsBroadcast) -> Router {
    Router::new()
        // Full pages (serve fragment or full page based on HX-Request)
        .route("/", get(routes::dashboard))
        .route("/functions", get(routes::functions_page))
        .route("/stdlib", get(routes::stdlib_page))
        .route("/dag", get(routes::dag_page))
        .route("/graph", get(routes::graph_editor_page))
        // API fragments (htmx search/polling)
        .route("/api/functions", get(routes::functions_api))
        .route("/api/activities", get(routes::activities_api))
        .route("/api/stdlib/{name}", get(routes::stdlib_source))
        .route("/api/dag/render", post(routes::dag_render))
        // Graph editor API
        .route("/api/graph/palette", get(routes::graph_palette))
        .route("/api/graph/compile", post(routes::graph_compile))
        .route("/api/graph/eval", post(routes::graph_eval))
        .route("/api/graph/plot", post(routes::graph_plot))
        // Metrics ingestion (push from running sessions)
        .route("/api/v1/import/prometheus", post(routes::ingest_prometheus))
        // Dynamic controls (SRD 23). Read-side lists every
        // declared control in the running session's tree;
        // write-side dispatches a non-blocking f64 write
        // tagged with `ControlOrigin::Api { source }`.
        .route("/api/controls", get(routes::list_controls))
        .route("/api/control/{name}", post(routes::set_control))
        // Scenario / scope tree (SRD 18b). Returns the running
        // session's scene tree as JSON for renderers that want
        // hierarchical structure (collapsing scopes, scope-level
        // aggregate status, etc.).
        .route("/api/scope-tree", get(routes::scope_tree))
        // WebSocket metric stream
        .route("/ws/metrics", get(ws::metrics_ws))
        // Shared state & middleware
        .with_state(broadcast)
        .layer(middleware::from_fn(no_cache_headers))
        .layer(CompressionLayer::new())
}

/// Middleware that sets aggressive no-cache headers on every response.
async fn no_cache_headers(request: Request<axum::body::Body>, next: middleware::Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "Cache-Control",
        "no-cache, no-store, must-revalidate".parse().unwrap(),
    );
    headers.insert("Pragma", "no-cache".parse().unwrap());
    headers.insert("Expires", "0".parse().unwrap());
    response
}

/// Start the web server on the given port, on loopback only: the
/// dashboard carries no authentication, so exposing it beyond the
/// host is an explicit `bind=` choice, never the default.
///
/// Standalone mode — no live metrics, just browsing tools.
pub async fn serve(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let broadcast = MetricsBroadcast::new(16);
    serve_with(
        std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        broadcast,
    )
    .await
}

/// Start the web server with a pre-configured broadcast channel.
///
/// Used in embedded mode where the metrics scheduler feeds the
/// broadcast reporter.
pub async fn serve_with(
    addr: std::net::SocketAddr,
    broadcast: MetricsBroadcast,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_router(broadcast);
    eprintln!("nmbrs web: listening on http://{addr}");
    let socket = socket2::Socket::new(
        if addr.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        },
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    let listener = tokio::net::TcpListener::from_std(socket.into())?;
    axum::serve(listener, app).await?;
    Ok(())
}
