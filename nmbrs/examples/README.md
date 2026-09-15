# nmbrs Examples

## workloads/

Example workload YAML files, grouped by theme. All support
`#!/usr/bin/env nmbrs` shebangs — make them executable and run directly, or run
any by its catalog name (e.g. `nmbrs run workload=examples/optimizer/sweep`).

Each declares its own verification rules (`#@` comments or a `verify:` block);
`nmbrs check nmbrs/examples/workloads/` runs and verifies them all, and
`nmbrs check nmbrs/examples/workloads/<theme>/` checks one theme. Use the same command
on your own workloads — see
[docs/guide/checking_workloads.md](../docs/guide/checking_workloads.md).

The themes read top-to-bottom as a rough learning path. Files marked
*(fixture)* are exhaustive coverage matrices driven by a paired Rust test —
runnable, but the per-theme demos above them are the better starting point.

### getting_started/ — the on-ramp
- `basic_workload` — minimal multi-op workload with schema setup
- `inline_ops` — a stdout workload and the byte-identical `op=` one-liner it equals (ratio prefixes)
- `polydat_bindings` — native Polydat DAG binding syntax
- `op_inline_forms` — how `op=` is detected and de-sugared, with the YAML equivalent
- `feature_showcase` — a tour of phases, scenarios, params, conditions

### iteration/ — comprehensions, control flow, conditionals
- `cartesian_space` — mixed-radix 3D coordinate decomposition
- `control_flow` — `do_while` / `do_until` at scenario level
- `conditional_ops` — `if:` field for per-cycle op skipping
- `comprehension_coverage` — comprehension-surface matrix *(fixture)*

### cursors/ — cursor enumeration & partitioning
- `all_cursor/` — `enumerate` / `every_tenth` / `first_five`: the `all(<cursor>)` generator
- `timeboxed_partition_sweep` — partition-bound open-extent sweep
- `cursor_partitions_coverage` — SRD-71 partition-shape matrix *(fixture)*

### scope/ — scenario tree, scope, params, shared state
- `scenario_includes` — `scenario: <name>` phase reuse
- `scenario_param_overrides/` — the `set:` / `bindings:` shadowing forms (7 demos)
- `scenario_set_iter_var/` — `set:` shadows reading an outer `for_each` iter-var (6 demos)
- `shared_cells` — `shared X := <literal>` round-trip
- `shared_pick_through_for_each` — scope-chain detect-then-pick
- `scope_coverage` — scope-model matrix *(fixture)*

### optimizer/ — SRD-86 optimization (see [docs/guide/optimizer.md](../docs/guide/optimizer.md))
- `sweep` — discrete axis, default `sweep` (identity) + best-selection
- `continuous` — continuous (float-range) axis sampled by `nelder_mead`
- `categorical` — categorical (named-label) axis
- `multiaxis` — multi-axis continuous search with `cmaes`
- `inline_objective` — objective written as an inline polydat expression
- `saturation` — run-produced objective (metric reader), settled across the run
- `metricsql` — run-produced objective via a MetricsQL reader, settled
- `control` — `servo:` a live control, retargeted by a daemon on one continuous phase
- `multiservo` — `servo: [concurrency, rate]`, two live controls over a 2-D grid
- `hybrid` — mixed coordinate + control axes in one node

### metrics/ — synthetic metrics, summaries, reports
- `synthetic_metrics` — formula-driven SRD-40b synthetic metrics
- `summary/` — `basics` / `aggregates` / `cli_override` / `gk_context` / `multi_phase` / `sidecar_demo`
- `report_text_file_demo` — `text` + `file` report item kinds
- `reports_coverage` — unified `report:` block matrix *(fixture)*

### expressions/ — polydat operators & stdlib
- `math_and_bitwise` — all infix operators, auto-widening
- `json_param` — JSON / map-literal workload params
- `stdlib_coverage` — stdlib node-catalog matrix *(fixture)*

### controls/ — runtime control & lifecycle
- `dynamic_controls` — SRD-23 dynamic control demo
- `phase_poll_smoke` — SRD-75 phase-poll (needs a backend)
- `stop_conditions_coverage` — SRD-83 stop-condition matrix *(fixture)*

### modeling/ — realistic workload shapes
- `service_model` — multi-table service sim with ratios + `delay:` latency injection
- `capture_flow` — capture flow between ops

### visual/ — rendered terminal patterns
- `heatmap` · `lissajous_plot` · `maze` · `polar_rose` · `spirograph`
- `distribution/` — the same three distributions three ways: `histogram` (binned bars), `scatter` (value vs. index, stacked lanes), `overlaid` (one shared axis)

### signals/ — signal generation & analysis
- `fourier_analysis` — FFT of a fractal noise signal
- `lfsr` — Galois linear-feedback shift register with bitwise Polydat ops

### diagnostics/ — by-design edge/error demos
- `diag_unresolved` — unresolved-`{placeholder}` error repro
- `resume_test` — resumable-workload staircase fixture

## modules/

GK module files (`.polydat`) and workloads that exercise them.

- `hashed_id.polydat` — Example Polydat module (deterministic hashed ID)
- `euler_circuit.polydat` — Euler circuit Polydat module
- `module_test.yaml` — Adjacent `.polydat` file resolution
- `stdlib_test.yaml` — Embedded stdlib module resolution
