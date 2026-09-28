# nmbrs

`nmbrs` is a command-line workload generator and benchmarking tool, a Rust
rewrite of [nosqlbench](https://github.com/nosqlbench/nosqlbench). You describe
operations in a YAML workload (or a one-line `op=` template), and nmbrs renders
and executes them against an adapter (stdout, HTTP, CQL, a simulated service)
with rate limiting, concurrency control, phased scenarios, and latency metrics.

Every generated value is derived from a cycle number through a DAG of
functions, so the same cycle always produces the same output and runs are
reproducible.

## Where it sits in nmbrs

This crate is the `nmbrs` binary, the user-facing entry point. It pulls in
the rest of the workspace:

- [polydat](https://crates.io/crates/polydat): the data-generation kernel
  (binding DAGs, node functions, stdlib modules, JIT)
- [nmbrs-workload](https://crates.io/crates/nmbrs-workload): workload YAML
  parsing, bind points, phasing
- [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime): async execution
  engine and adapter contract
- [nmbrs-metrics](https://crates.io/crates/nmbrs-metrics) and
  [nmbrs-metricsql](https://crates.io/crates/nmbrs-metricsql): HDR latency
  histograms, the per-session SQLite metrics db, and MetricsQL queries over it
- [nmbrs-rate](https://crates.io/crates/nmbrs-rate),
  [nmbrs-errorhandler](https://crates.io/crates/nmbrs-errorhandler),
  [nmbrs-optimizers](https://crates.io/crates/nmbrs-optimizers): rate
  limiting, error routing, parameter optimizers
- [nmbrs-tui](https://crates.io/crates/nmbrs-tui) and
  [nmbrs-web](https://crates.io/crates/nmbrs-web): terminal and browser
  dashboards
- Adapters: [nmbrs-adapter-stdout](https://crates.io/crates/nmbrs-adapter-stdout),
  [nmbrs-adapter-http](https://crates.io/crates/nmbrs-adapter-http),
  [nmbrs-adapter-cql](https://crates.io/crates/nmbrs-adapter-cql),
  [nmbrs-adapter-testkit](https://crates.io/crates/nmbrs-adapter-testkit),
  [nmbrs-adapter-plotter](https://crates.io/crates/nmbrs-adapter-plotter),
  and optionally [nmbrs-adapter-openapi](https://crates.io/crates/nmbrs-adapter-openapi)

## Install

```
cargo install nmbrs
```

The default build uses only pure-Rust dependencies and needs no C/C++
toolchain. Besides `nmbrs`, the package also installs two auxiliary tools
for vector-search datasets: `vectordata-audit` and `trace-recall`.

Enable shell completion (bash; `nmbrs completions --shell <bash|zsh|fish|elvish|powershell>`
prints the script for a specific shell):

```
eval "$(nmbrs completions)"
```

## Quick start

Render five operations from an inline template, printed by the default
`stdout` adapter:

```
$ nmbrs run op='INSERT INTO t (id, name) VALUES ({{mod(hash(cycle), 1000000)}}, "{{number_to_words(cycle)}}")' cycles=5
INSERT INTO t (id, name) VALUES (607535, "zero")
INSERT INTO t (id, name) VALUES (822465, "one")
INSERT INTO t (id, name) VALUES (348110, "two")
INSERT INTO t (id, name) VALUES (139053, "three")
INSERT INTO t (id, name) VALUES (603978, "four")
```

`{{expr}}` is evaluated per cycle. A top-level `;` separates ops and an
`N:` prefix sets each op's ratio:

```
nmbrs run op='3:read id={{mod(hash(cycle),1000000)}};1:write id={{mod(hash(cycle),1000000)}}' cycles=8
```

Check that the installed binary works on this machine (built-in adapters
only, no external systems):

```
nmbrs run workload=selfcheck
```

### A workload file

```yaml
#!/usr/bin/env nmbrs
# service.yaml

params:
  keyspace: demo
  table: users
  user_count: "100000"

bindings: |
  input cycle: u64
  user_id := mod_wire(hash(cycle), user_count)
  user_name := number_to_words(mod(hash(hash(cycle)), 1000))
  is_write := mod(cycle, 5)

ops:
  read_user:
    ratio: 4
    stmt: "SELECT * FROM {keyspace}.{table} WHERE id={user_id}"
  write_user:
    ratio: 1
    if: is_write
    stmt: "INSERT INTO {keyspace}.{table} (id, name) VALUES ({user_id}, '{user_name}')"
```

```
nmbrs run workload=service.yaml cycles=100 concurrency=4 rate=1000
nmbrs service.yaml cycles=100 concurrency=4 rate=1000
```

With the `#!/usr/bin/env nmbrs` line and the executable bit set, the file
can also be run directly on Unix-like systems: `./service.yaml cycles=100`.

Each run writes a session directory under `./sessions/` (with
`sessions/latest` pointing at the newest) holding the run log and a SQLite
metrics database that the `metrics`, `report`, `replay` and `checkpoint`
commands read.

## Commands

`nmbrs --help` lists every subcommand; `nmbrs <subcommand> --help` shows its
flags. The main ones:

| Command | Purpose |
|---------|---------|
| `run` | Execute a workload (`workload=`, `op=`, `scenario=`, `cycles=`, `concurrency=`, `rate=`, adapter params) |
| `check` | Run a workload and verify it against its declared `#@` / `verify:` rules; non-zero exit on failure |
| `refine` | Run phases that are new or incomplete on top of an existing session |
| `describe` | Built-in documentation: `wiring`, `adapter`, `workloads`, `controls`, `optimizers`, `wrappers`, `op` |
| `copy` | Copy a bundled workload to a local file for editing |
| `metrics` | Inspect a session's metrics db (`list`, `summarize`, `last`, `match`, `groups`, `query`, `watch`) |
| `report` (`plot`, `table`) | Render the items in a workload's `report:` block |
| `wiring visualize` | Plot a binding expression in the terminal |
| `bench` | Micro-benchmarks (for example `bench wiring`) |
| `attach` | Connect to the introspection socket of a run started with `inspector=on` |
| `replay`, `checkpoint` | Walk readout snapshots / the checkpoint log of a session |
| `web` | Run the web dashboard |
| `completions` | Print the shell-completion script |

### Running workloads

```
nmbrs run workload=service.yaml cycles=1M concurrency=8 rate=10000
nmbrs run workload=examples/modeling/service_model adapter=testkit cycles=2K
nmbrs check examples/getting_started/inline_ops
```

Workloads are referenced by file path or by bundled catalog name. The
examples from the repository are bundled into the binary:

```
nmbrs describe workloads          # curated workloads
nmbrs describe workloads --all    # also the bundled examples
nmbrs describe workloads selfcheck
nmbrs copy selfcheck              # copy one out for editing
```

The live display is chosen by `tui=`: an interactive terminal gets a
line-mode status display by default, `tui=on` switches to the full-screen
dashboard, and `tui=off` gives plain log output.

### Exploring the data wiring

```
nmbrs describe wiring functions
nmbrs describe wiring stdlib
nmbrs bench wiring 'hash(cycle)' cycles=1M threads=1:8*2
```

`wiring visualize` evaluates an expression across a range of cycles and plots
it in the terminal. Output wire names pick the plot mode: plain names plot
against the cycle, `x`/`y` give a parametric plot, `r`/`theta` a polar plot
(override with `--mode=plot|parametric|polar`):

```
nmbrs wiring visualize 'y := sin(to_f64(cycle) * 0.1)' cycles=200
nmbrs wiring visualize 't := to_f64(cycle)*0.06; x := cos(t); y := sin(t)' cycles=120
nmbrs wiring visualize 'theta := to_f64(cycle)*0.06; r := cos(theta*3.0)' cycles=120
```

### Metrics and reports

```
nmbrs metrics list                                  # families and label dimensions
nmbrs metrics query 'rate(cycles_total[1m])'        # MetricsQL against the latest session
nmbrs report list                                   # report items of the latest session
nmbrs report all                                    # re-render them
```

A workload's `report:` items are rendered at the end of each run.
`--report-openmetrics-to=<url>` additionally pushes metrics to an
OpenMetrics/Prometheus endpoint.

### Web dashboard

```
nmbrs web                  # http://127.0.0.1:8080, loopback only
nmbrs web --port 9090
nmbrs web --daemon         # Unix only; stop with `nmbrs web --stop`
```

## CQL (Cassandra / ScyllaDB)

The `cql` adapter speaks the Apache Cassandra wire protocol and works against
Apache Cassandra and ScyllaDB. Bundled CQL workloads are listed by
`nmbrs describe workloads` (for example `cql/baselinesv3/keyvalue`,
`cql/baselinesv3/tabular`, `cql/baselinesv3/timeseries`):

```
nmbrs run workload=cql/baselinesv3/keyvalue host=127.0.0.1
nmbrs describe adapter=cql      # engines linked into this binary and their params
```

Two engines are available, selected by Cargo features:

| Feature | Engine | Notes |
|---------|--------|-------|
| `engine-scylla` (default) | [scylla](https://crates.io/crates/scylla), pure Rust | No native dependencies |
| `engine-cassandra-cpp` | Apache Cassandra C++ driver via FFI | Needs native libraries at link time, see below |
| `all-engines` | both | Pick at runtime with `cqldriver=scylla` or `cqldriver=cassandra-cpp`; cassandra-cpp is the default when both are linked |

### Building with the cassandra-cpp engine

The cassandra-cpp engine links statically against the Apache Cassandra C++
driver (`libcassandra`), which in turn needs libuv, OpenSSL and zlib. These
must be available when nmbrs is linked, and these environment variables must
point at them:

- `CASSANDRA_SYS_LIB_PATH`: directory containing the static `libcassandra`
- `LIBRARY_PATH`: search path including that directory
- `C_INCLUDE_PATH`: include path containing the driver headers

```
CASSANDRA_SYS_LIB_PATH=/path/to/sysroot/lib \
LIBRARY_PATH=/path/to/sysroot/lib \
C_INCLUDE_PATH=/path/to/sysroot/include \
cargo install nmbrs --no-default-features --features engine-cassandra-cpp
```

Use `--features all-engines` instead to get both engines in one binary.

From a git checkout, [crates/nmbrs-adapter-cql/build.sh](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs-adapter-cql/build.sh)
builds the driver from source (in Docker by default, or on the host with
`DRIVER_BUILD_MODE=native`) into `crates/nmbrs-adapter-cql/target/sysroot/`, then builds
nmbrs against it with the variables above set:

```
cd crates/nmbrs-adapter-cql
bash build.sh driver     # build only the C++ driver into target/sysroot
bash build.sh            # driver, then nmbrs with engine-cassandra-cpp
bash build.sh install    # cargo install --path crates/nmbrs --features all-engines
```

## Cargo features

| Feature | Default | Effect |
|---------|---------|--------|
| `engine-scylla` | yes | Pure-Rust CQL engine |
| `engine-cassandra-cpp` | no | Apache Cassandra C++ driver CQL engine (native libraries required) |
| `all-engines` | no | Both CQL engines |
| `openapi` | no | Adds `describe-openapi spec=<file>` and `run-openapi spec=<file> [base_url=...] [adapter=...]`, which synthesize HTTP ops from an OpenAPI 3.x spec |
| `flamegraph` | no | Enables the in-process sampling profiler in nmbrs-runtime (`profiler=flamegraph`) |

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- Example workloads: https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs/examples
- Getting started guide: https://github.com/nosqlbench/nmbrs/blob/main/docs/guide/getting_started.md
- Checking workloads: https://github.com/nosqlbench/nmbrs/blob/main/docs/guide/checking_workloads.md
- Optimizer guide: https://github.com/nosqlbench/nmbrs/blob/main/docs/guide/optimizer.md

## License

Apache-2.0
