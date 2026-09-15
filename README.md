# nmbrs

[![build](https://github.com/nosqlbench/nmbrs/actions/workflows/build.yml/badge.svg)](https://github.com/nosqlbench/nmbrs/actions/workflows/build.yml)

High-performance workload generation and database testing in Rust.

nmbrs generates deterministic, reproducible request streams at scale.
Every value is derived from a cycle number through a composable DAG
of functions — same cycle, same output, every time. This makes
workloads debuggable, cacheable, and exactly reproducible across runs.

Part of the [nosqlbench](https://github.com/nosqlbench/nosqlbench) project.

This system was derived from things learned building nosqlbench, and shares many of its concepts. However, some things were kept and some removed. This will be a much leaner and meaner version of what nosqlbench is. It will do some things differently.

## Quick Start

```
$ nmbrs run op='INSERT INTO t (id, name) VALUES ({{mod(hash(cycle), 1000000)}}, "{{number_to_words(cycle)}}")' cycles=5

INSERT INTO t (id, name) VALUES (527897, "zero")
INSERT INTO t (id, name) VALUES (460078, "one")
INSERT INTO t (id, name) VALUES (564547, "two")
INSERT INTO t (id, name) VALUES (960189, "three")
INSERT INTO t (id, name) VALUES (862456, "four")
```

Or from a workload file:

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
$ chmod +x service.yaml
$ ./service.yaml cycles=100 concurrency=4 rate=1000
```

## Features

**Generation Kernel (Polydat)** — A DAG-based data generation engine with:
- Infix operators (`+`, `-`, `*`, `/`, `%`, `**`, `&`, `|`, `^`, `<<`, `>>`)
- 100+ node functions: hash, distributions, noise, strings, vectors, CSV/JSONL
- Type-aware dispatch with auto-widening (u64/f64/string)
- Constant folding, provenance-based invalidation, JIT compilation
- Module system with composable `.polydat` files and stdlib

**Workload Engine** — Flexible execution with:
- Phased workloads (schema → rampup → steady-state)
- Conditional ops (`if:` field skips per-cycle)
- Latency injection (`delay:` field for GK-driven think time)
- Ratio-weighted op sequencing
- Capture flow between ops within a stanza
- Polydat expressions in config (`cycles="{vector_count("example")}"`)

**Adapters** — Protocol drivers for:
- stdout (debugging, dry-run, format=json/csv/stmt)
- HTTP (REST APIs, configurable timeouts)
- CQL (Cassandra/ScyllaDB — `cassandra-cpp` or `scylla` driver)
- testkit (simulated service: latency, errors, capacity limits)

**Observability** — Built-in metrics and dashboards:
- HDR histograms for latency percentiles
- OpenMetrics push to Prometheus/VictoriaMetrics
- Live TUI dashboard (`--tui`)
- Web dashboard (`nmbrs web`)

## Build

```
cargo build --release
```

Enable shell completions:
```
eval "$(nmbrs completions)"
```

## Commands

```
nmbrs run workload=file.yaml cycles=1M concurrency=8 rate=10000
nmbrs run op='hello {{hash(cycle)}}' cycles=10
nmbrs bench wiring 'hash(cycle)' cycles=1M threads=1:8*2
nmbrs describe wiring functions
nmbrs report plot workload=file.yaml   # render the workload's report: items
nmbrs metrics list                     # introspect a session's metrics db
nmbrs web --daemon
```

## Data Wiring

The _wiring_ for nmbrs is provided by a dedicated subsystem called polydat.
This takes the place of the classic *virtual dataset* procedural generation 
system in nosqlbench, but it is, like nmbrs, derived and evolved from the
original blueprints around lessons learned in that project.

### Visualizing wiring expressions

`nmbrs wiring visualize <expr>` evaluates a wiring expression across a
range of cycles and plots it in the terminal. The **output wire names**
choose the plot mode:

```
# default — each output plotted against cycle (line plot)
nmbrs wiring visualize 'y := sin(to_f64(cycle) * 0.1)' cycles=200

# wires named x / y → parametric (this traces a circle)
nmbrs wiring visualize 't := to_f64(cycle)*0.06; x := cos(t); y := sin(t)' cycles=120

# wires named r / theta → polar (this traces a 3-petal rose)
nmbrs wiring visualize 'theta := to_f64(cycle)*0.06; r := cos(theta*3.0)' cycles=120
```

`r`/`theta` also accept `radius`/`angle` or `rho`/`phi`. Pass
`--mode=plot|parametric|polar` to override the name inference.

`wiring visualize` is sugar for `nmbrs run adapter=plotter render=single` —
it builds a one-op plotter workload and runs it through the engine. The
plotter adapter renders either a **single** static snapshot (the default
for `visualize`, and for any non-TTY output) or **live**, animated as
cycles arrive — `nmbrs run adapter=plotter op='…' render=live` (or
`render=5hz` for a specific refresh rate; a TTY defaults to live).

## Examples

See [`nmbrs/examples/`](nmbrs/examples/):
- [`workloads/`](nmbrs/examples/workloads/) — runnable workload YAMLs: phases,
  conditional ops (`if:`), delays, scenarios, cursors & partitions,
  capture flow, comprehensions, dynamic controls, and reports
- [`modules/`](nmbrs/examples/modules/) — the Polydat module system
  (`.polydat` files imported by module workloads)

## Architecture

```
polydat     Polydat engine: DAG compilation, node functions, JIT, provenance
nmbrs-workload     YAML parsing, bind points, inline expressions, phasing
nmbrs-runtime     Async execution engine, dispenser wrappers, capture flow
nmbrs-metrics      HDR histograms, frame capture, OpenMetrics export
nmbrs-rate         Async token bucket rate limiter
nmbrs-errorhandler Composable error routing
nmbrs           CLI binary (nmbrs), bench, plot, web dashboard
nmbrs-tui          Terminal UI for live monitoring
nmbrs-web          Web dashboard with Axum + HTMX
```

## Functional Areas

Each of these areas has a distinctive design which has evolved from its 
nosqlbench form. This represents a capsule-form view of the user-facing 
elements which compose together to make a whole system. Each of these should 
be considered a separate, modular subsystem.

They are due for some documentation.

- user interface (readout, output channels, TUI)
  - TUI status views (readout)
  - CLI dynamic completion help
- test data / variates (polydat, data access libraries)
- custom op behaviors (op wrapping facility and yaml alignment)
- workload description (session/workload/scenario/phase)
- dynamic controls (realtime hooks, api, usage examples)
- reports / plots / tables (per-session, per-workload, ...)
- metrics db / metricsql (stores, query interface, uniformity)
- portability / local-only sufficiency
- safety checks

## License

Apache-2.0
