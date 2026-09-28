# nmbrs-web

`nmbrs-web` is the browser dashboard for [nmbrs](https://crates.io/crates/nmbrs),
built on [axum](https://crates.io/crates/axum), [askama](https://crates.io/crates/askama)
templates and htmx. It serves pages for browsing the polydat function
reference and stdlib modules, rendering binding DAGs, an interactive graph
editor, a live metrics view fed over a WebSocket, and JSON endpoints for
dynamic controls and the scenario tree. Most users reach it through the
`nmbrs web` command rather than depending on this crate directly.

## Where it sits in nmbrs

The [nmbrs](https://crates.io/crates/nmbrs) binary uses this crate to
implement `nmbrs web`. It depends on
[nmbrs-metrics](https://crates.io/crates/nmbrs-metrics) (metric snapshots and
the reporter trait), [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime)
(dynamic controls and scene tree of an in-process run) and
[polydat](https://crates.io/crates/polydat) (function catalog, stdlib, graph
compilation and evaluation).

## Using it from the CLI

```
cargo install nmbrs
nmbrs web                  # listens on http://127.0.0.1:8080
nmbrs web --port 9090
nmbrs web --bind 0.0.0.0   # expose beyond loopback; there is no authentication
nmbrs web --daemon         # Unix only; manage with --stop / --restart
```

## Routes

| Path | Purpose |
|------|---------|
| `GET /` | Dashboard |
| `GET /functions` | Polydat function reference (`GET /api/functions` for search) |
| `GET /stdlib` | Stdlib `.polydat` module browser (`GET /api/stdlib/{name}` for source) |
| `GET /dag` | DAG visualization (`POST /api/dag/render`) |
| `GET /graph` | Interactive graph editor (`/api/graph/palette`, `compile`, `eval`, `plot`) |
| `GET /api/activities` | Activity table fragment |
| `GET /api/controls` | List dynamic controls |
| `POST /api/control/{name}` | Write a control value |
| `GET /api/scope-tree` | Scenario / scope tree as JSON |
| `POST /api/v1/import/prometheus` | Accept a Prometheus/OpenMetrics text push and forward it to WebSocket clients |
| `GET /ws/metrics` | WebSocket stream of metric frames (HTML fragments for htmx) |

`/api/controls` and `/api/scope-tree` read the session state of a run in the
same process. With the standalone `nmbrs web` server no run is active, so they
return an empty list and `installed: false` respectively.

## Library API

- `server::build_router(broadcast: ws::MetricsBroadcast) -> axum::Router`:
  the full router with all routes, a no-cache middleware and gzip
  compression. Use it to mount the dashboard in your own axum server.
- `server::serve(port: u16)`: serve the router on `127.0.0.1:<port>` with an
  empty broadcast channel.
- `server::serve_with(addr: SocketAddr, broadcast: MetricsBroadcast)`: serve
  on a chosen address with a broadcast you control. This is what `nmbrs web`
  calls.
- `ws::MetricsBroadcast`: the broadcast channel behind `/ws/metrics`.
  `MetricsBroadcast::new(capacity)` creates it, `publish(MetricSet)` sends a
  frame to all connected clients, and `reporter()` returns a
  `BroadcastReporter` that implements `nmbrs_metrics::scheduler::Reporter`, so
  a metrics scheduler can feed the dashboard directly.
- `routes`, `graph`, `models`: the page and API handlers, the graph-editor
  backend (`build_palette`, `compile_graph`, `eval_graph`, `plot_graph`), and
  the view-model types shared by the templates and JSON endpoints.

Minimal standalone server:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    nmbrs_web::server::serve(8080).await
}
```

Serving with a broadcast you publish into:

```rust
use nmbrs_web::{server, ws::MetricsBroadcast};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let broadcast = MetricsBroadcast::new(16);
    // Keep a clone to call `broadcast.publish(snapshot)`, or register
    // `broadcast.reporter()` with an nmbrs-metrics scheduler.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8080));
    server::serve_with(addr, broadcast.clone()).await
}
```

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- Crate source: https://github.com/nosqlbench/nmbrs/tree/main/nmbrs-web
- Web UI design notes: https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/54_web_ui.md
- API docs: https://docs.rs/nmbrs-web

## License

Apache-2.0
