# Local changes to the vendored `cassandra-cpp` fork

This is a fork of upstream `cassandra-cpp` 3.0.2. It exists so the nmbrs
CQL adapter can reach a few driver capabilities the published wrapper
does not expose. Keep this file current when the local surface changes —
`adapters/cql/Cargo.toml` points here.

## Added surface

- **Server-side trace capture.** Upstream does not expose a
  `cass_future_tracing_id` retrieval path (`Protected::build` /
  `Completable` lack the public surface), and the trace id is required
  for any meaningful `system_traces` capture. The fork adds the small
  surface needed to read it off a completed future.

- **`Statement::set_statement_request_timeout`** — per-statement request
  timeout (`cass_statement_set_request_timeout`). Upstream exposes only
  the cluster-level `Cluster::set_request_timeout`.

- **`Batch::set_request_timeout`** — per-batch request timeout
  (`cass_batch_set_request_timeout`). Required for correct batch
  behaviour: `cass_session_execute_batch` reads its timeout from the
  batch, and the driver IGNORES the per-statement timeouts of the
  statements added to it. Without this a batch always uses the
  cluster-default request timeout (12s) regardless of the op's declared
  `timeout:` / `request_timeout_ms`, so a batch times out client-side
  well before a longer server-side coordinator timeout — the client and
  server disagree on when a request has failed.

## Changed surface

- **`ErrorKind::CassErrorResult` carries a `Box<CassErrorDetails>`.**
  Upstream spreads the eleven fields of a detailed error result across
  the variant itself, which makes every `Error`, and so every
  `Result<_, Error>` the driver returns, at least 160 bytes. The fork
  moves them into the named struct `CassErrorDetails` behind a box, so
  the common `Ok` path stays small. Match on the variant and read the
  fields by name (`details.code`, `details.message`, ...).
