# nmbrs-adapter-cql

The `cql` adapter for [nmbrs](https://crates.io/crates/nmbrs). It runs
workload ops as CQL statements against Apache Cassandra or ScyllaDB. The
adapter can use either of two driver engines, chosen with Cargo features:

| Feature | Engine | Native libraries needed | Builds from crates.io as-is |
|---------|--------|-------------------------|-----------------------------|
| `engine-scylla` (default) | [scylla](https://crates.io/crates/scylla) 1.6, pure Rust | none | yes |
| `engine-cassandra-cpp` | [nmbrs-cassandra-cpp](https://crates.io/crates/nmbrs-cassandra-cpp), a wrapper over the Apache Cassandra C++ driver | static libcassandra, plus libuv, OpenSSL, zlib | no; see "Building the cassandra-cpp engine" below |
| `all-engines` | both | as above | no |

Both engines use the CQL native protocol and work with both Cassandra and
ScyllaDB.

## Using it

This adapter is used through the `nmbrs` CLI. Select it with `adapter=cql`.
`cql` is the only adapter name this crate registers. The engines are drivers
behind it, not separate adapters, so `adapter=scylla` is not valid.

```bash
nmbrs run workload=crates/nmbrs/workloads/cql/baselinesv3/keyvalue.yaml host=10.0.0.5

# Pick an engine explicitly, in a binary that has both:
nmbrs run workload=... adapter=cql cqldriver=scylla
nmbrs run workload=... adapter=cql cqldriver=cassandra-cpp
```

If `cqldriver=` is not given and both engines are linked, `cassandra-cpp` is
used (it has the lower rank, 100 against 200). With only one engine linked,
that engine is used.

### Connection parameters

Set these on the command line (`key=value`) or in the workload's `params:`.
Durations are strings like `60s` or `500ms`, or a bare number of seconds.

| Param | Default | Effect |
|-------|---------|--------|
| `cqldriver` | lowest rank | `scylla` or `cassandra-cpp`. |
| `hosts` (alias `host`) | `127.0.0.1` | Contact points, separated by commas. |
| `port` | `9042` | |
| `keyspace` | none | Keyspace for the session. |
| `connect_keyspace` | | Overrides `keyspace` for the connection only. Use `connect_keyspace=""` to connect without a keyspace (for DDL) while `{keyspace}` in statements still takes the `keyspace` param. |
| `consistency` | `LOCAL_ONE` | Default consistency: `ANY`, `ONE`, `TWO`, `THREE`, `QUORUM`, `ALL`, `LOCAL_QUORUM`, `EACH_QUORUM`, `LOCAL_ONE`. |
| `username`, `password` | none | Authentication. |
| `timeout` / `request_timeout` | `12s` | Default request timeout (duration). `request_timeout` wins over `timeout`. |
| `request_timeout_ms` | `12000` | The same setting in milliseconds. Used only when neither duration form is set. |
| `connect_timeout` | `5s` | Timeout for establishing connections. |
| `heartbeat_interval` | `30s` | Connection heartbeat interval. The scylla engine uses it as its CQL keepalive interval. |
| `connection_idle_timeout` | `60s` | How long a connection may go without a heartbeat response before it is closed. The scylla engine uses it as its keepalive timeout. |
| `reconnect_base_delay`, `reconnect_max_delay` | `2s`, `600s` | Exponential reconnect policy. Only cassandra-cpp uses these. The scylla engine logs a warning and ignores them. |
| `trace_rate` | `0` | Fraction of ops (0.0 to 1.0) that request server-side tracing. This sets the starting value of the `cql_trace_rate` dynamic control. cassandra-cpp only. |
| `trace_log` | session directory | Path of the trace output file. cassandra-cpp only. |
| `cassandra_log_level` | | cassandra-cpp driver log level: `DISABLED`, `CRITICAL`, `ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`. |

### Op fields

Each op must have a statement field. The first one found, in this order, sets
how the op is executed:

| Field | Execution |
|-------|-----------|
| `raw` or `simple` | Unprepared statement, executed directly. |
| `prepared` or `stmt` | Prepared once per op template, with the values bound on each cycle. |

Bind points (`{name}`) in a prepared statement become `?` markers. The bound
values are checked against the column types the cluster reports.

These optional fields change how a statement runs:

| Field | Effect |
|-------|--------|
| `consistency` | Consistency level for this op (same names as above). |
| `serial_consistency` | Serial consistency level for this op. |
| `timeout` | Request timeout for this op: a duration string, or a number of seconds. Wins over `request_timeout_ms`. |
| `request_timeout_ms` | Request timeout for this op, in milliseconds. |
| `page_size` | Result page size (positive integer). |
| `cql_trace` | `true` requests server-side tracing for this op. |
| `batch` | For `prepared`/`stmt` ops: send N rows per CQL `BATCH`. |
| `max_batch_size` | For `prepared`/`stmt` ops: keep each batch under a byte budget. With `batch`, N rows are split into sub-batches that each fit the budget. On its own, the row count is estimated from the size of the first row. |
| `batchtype` | `logged`, `unlogged` (default) or `counter`. |

Either `batch` or `max_batch_size` turns the op into a batch. `raw`/`simple`
ops are never batched.

An excerpt from [`crates/nmbrs/workloads/cql/baselinesv3/keyvalue.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/workloads/cql/baselinesv3/keyvalue.yaml),
with its rampup phase and scenarios left out:

```yaml
params:
  host: localhost
  adapter: cql
  keyspace: baselines
  table: keyvalue
  replication: "{'class': 'SimpleStrategy', 'replication_factor': '1'}"
  read_cl: LOCAL_QUORUM
  write_cl: LOCAL_QUORUM
  concurrency: "100"
  keycount: "1000000"
  valuecount: "1000000000"
  main_ms: "60000"
  main_chunk: "100000"

phases:
  schema:
    concurrency: 1
    ops:
      create_keyspace:
        raw: |
          CREATE KEYSPACE IF NOT EXISTS {keyspace}
          WITH replication = {replication} AND durable_writes = true
      create_table:
        raw: |
          CREATE TABLE IF NOT EXISTS {keyspace}.{table} (
            key text,
            value text,
            PRIMARY KEY (key)
          )

  main:
    concurrency: "{concurrency}"
    bindings: |
      cursor op = until_elapsed(main_chunk, main_ms)
      rw_key   := format_u64(u64_mod(hash(op), is_positive(keycount)), 10)
      rw_value := format_u64(u64_mod(hash(hash(op)), is_positive(valuecount)), 10)
    ops:
      main_select:
        ratio: 5
        consistency: "{read_cl}"
        prepared: |
          SELECT * FROM {keyspace}.{table} WHERE key = '{rw_key}'
      main_insert:
        ratio: 5
        consistency: "{write_cl}"
        prepared: |
          INSERT INTO {keyspace}.{table}
          (key, value) VALUES ('{rw_key}', '{rw_value}')
```

More CQL workloads (tabular, time series, vector search, compaction) are in
[`crates/nmbrs/workloads/cql/`](https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs/workloads/cql).

### Polydat functions

The crate registers these Polydat functions:

- `cql_timeuuid(seed)` returns a deterministic version-1 `timeuuid` string for
  a `u64` seed.
- `cql_session(key)`, `cql_read_cached(session, name)`,
  `cql_read_current(session, name)` and `cql_server_batch_limit(session)` read
  cluster settings through the pooled session. For example,
  `max_batch_size: cql_server_batch_limit(cql_session(cql_session_key))` sizes
  batches to 90% of the server's `batch_size_fail_threshold`.

## Cargo features

- `engine-scylla` (default) builds the pure-Rust engine. It needs no C or C++
  toolchain.
- `engine-cassandra-cpp` builds the engine that uses the Apache Cassandra C++
  driver through FFI, via
  [nmbrs-cassandra-cpp](https://crates.io/crates/nmbrs-cassandra-cpp), a fork
  of `cassandra-cpp` 3.0.2 that adds retrieval of server-side trace ids, and
  [cassandra-cpp-sys](https://crates.io/crates/cassandra-cpp-sys). It is the
  only engine that declares the `cql_trace_rate` dynamic control.
- `all-engines` enables both.

At least one engine feature must be enabled, or the crate fails to compile.
The `nmbrs` crate forwards the same three feature names.

## Building the cassandra-cpp engine

At link time, `engine-cassandra-cpp` needs a static build of the Apache
Cassandra C++ driver (`libcassandra`), plus libuv, OpenSSL (`libssl`,
`libcrypto`) and zlib. `build.rs` adds `-lz` because recent driver builds bundle
minizip. None of these libraries come with the crate. Before building, point
the build at a directory containing `libcassandra.a` and `cassandra.h`:

```bash
export CASSANDRA_SYS_LIB_PATH=/path/to/sysroot/lib
export LIBRARY_PATH=/path/to/sysroot/lib:$LIBRARY_PATH
export C_INCLUDE_PATH=/path/to/sysroot/include:$C_INCLUDE_PATH

cargo install nmbrs --no-default-features --features all-engines
```

If the driver build installed only `libcassandra_static.a`, add a
`libcassandra.a` link to it: `cassandra-cpp-sys` links `-lcassandra`.

### Using `build.sh` from a git checkout

The repository's
[`crates/nmbrs-adapter-cql/build.sh`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs-adapter-cql/build.sh)
builds driver 2.17.1 from source and places the static library and header in
`crates/nmbrs-adapter-cql/target/sysroot/`. The workspace `.cargo/config.toml` already
points `LIBRARY_PATH`, `C_INCLUDE_PATH` and `CASSANDRA_SYS_*` there. The script
works only inside a checkout of the repository, so it does not help when
installing from crates.io.

```bash
git clone https://github.com/nosqlbench/nmbrs && cd nmbrs/adapters/cql
bash build.sh           # build the driver, then nmbrs with engine-cassandra-cpp
bash build.sh driver    # only the driver, into target/sysroot/
bash build.sh cargo     # only nmbrs (the sysroot must exist)
bash build.sh install   # cargo install --path nmbrs --features all-engines
bash build.sh docker    # build nmbrs entirely inside Docker
bash build.sh clean     # cargo clean for this crate, and remove the Docker images
```

The driver is built in Docker by default (`DRIVER_BUILD_MODE=docker`), on an
`ubuntu:<VERSION_ID>` base image taken from the host's `/etc/os-release`
(`ubuntu:22.04` if that file is missing). `DRIVER_BUILD_MODE=native`
builds it on the host (Linux or macOS). That needs cmake, git, make,
pkg-config, libuv and OpenSSL, and `CASSANDRA_CPP_DRIVER_VERSION` selects the
driver git ref.

## Where it sits

- Implements `DriverAdapter` and `OpDispenser` from
  [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime). It registers the
  `cql` adapter via `inventory`, and each engine registers as a driver of it.
  Phases with the same hosts, port, keyspace and credentials share one session.
- Reads op templates from
  [nmbrs-workload](https://crates.io/crates/nmbrs-workload) and reports metrics
  through [nmbrs-metrics](https://crates.io/crates/nmbrs-metrics).
- Source layout: `src/common/` is shared by both engines (config, consistency,
  op modes, per-op fields, Polydat functions, and the `cql` registration).
  `src/scylla/` and `src/cassandra_cpp/` hold the two engines.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-adapter-cql
- nmbrs CLI: https://crates.io/crates/nmbrs

## License

Apache-2.0
