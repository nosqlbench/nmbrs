# nmbrs-runtime

The workload execution runtime for nmbrs. It takes a parsed workload,
compiles its Polydat bindings into a tree of scope kernels, walks the
scenario tree, runs phases as activities of concurrent fibers, dispatches ops
through adapters and wrapper stacks, and records metrics into a session. It
also defines the adapter API (`DriverAdapter` / `OpDispenser`) that protocol
drivers implement. That API is the main reason to depend on this crate
directly.

## Where it sits in nmbrs

- Depends on:
  - [`nmbrs-workload`](https://crates.io/crates/nmbrs-workload) for the
    workload model
  - [`nmbrs-metrics`](https://crates.io/crates/nmbrs-metrics) for components,
    instruments, controls and metrics storage
  - [`nmbrs-rate`](https://crates.io/crates/nmbrs-rate) for rate limiting
  - [`nmbrs-errorhandler`](https://crates.io/crates/nmbrs-errorhandler) for
    error routing
  - [`polydat`](https://crates.io/crates/polydat), the data generation kernel
- Depended on by:
  - the [`nmbrs`](https://crates.io/crates/nmbrs) CLI
  - [`nmbrs-tui`](https://crates.io/crates/nmbrs-tui) and
    [`nmbrs-web`](https://crates.io/crates/nmbrs-web)
  - [`nmbrs-optimizers`](https://crates.io/crates/nmbrs-optimizers), with
    its `runtime` feature
  - the adapter crates:
    [`nmbrs-adapter-stdout`](https://crates.io/crates/nmbrs-adapter-stdout),
    [`nmbrs-adapter-http`](https://crates.io/crates/nmbrs-adapter-http),
    [`nmbrs-adapter-cql`](https://crates.io/crates/nmbrs-adapter-cql),
    [`nmbrs-adapter-testkit`](https://crates.io/crates/nmbrs-adapter-testkit),
    [`nmbrs-adapter-plotter`](https://crates.io/crates/nmbrs-adapter-plotter)

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs) CLI
(`cargo install nmbrs`) and run workloads with `nmbrs run`. This crate is for
people who write adapters, or who embed the runner in their own binary.

## Writing an adapter

Adapters live in `nmbrs_runtime::adapter`. An adapter has two phases:

- **Init time.** `DriverAdapter::map_op` is called once per op template,
  before any cycle runs. It does the expensive work: validating fields,
  preparing statements, building binders. It returns a boxed `OpDispenser`.
- **Cycle time.** `OpDispenser::execute(cycle, ctx)` is called for every
  cycle. It resolves that cycle's values and performs the operation.

### `DriverAdapter`

Implement this trait on your adapter type. It is constructed once per
activity and shared across fibers through an `Arc`.

| Method | Required | Purpose |
|--------|----------|---------|
| `name(&self) -> &str` | yes | The adapter name, e.g. `"http"`. |
| `map_op(&self, template: &ParsedOp, parent: Arc<dyn Kernel>) -> MapOpFuture` | yes | Build the dispenser for one op template. `parent` is the phase's Polydat scope kernel. |
| `known_op_fields()` | no | Return `Some(&[...])` to declare your op-field vocabulary. The core then rejects templates with unknown fields. `None` (the default) is permissive. |
| `known_op_params()` | no | Extra keys allowed under an op's `params`. |
| `default_status_metrics()` | no | `StatusMetric`s shown on the status line. |
| `display_preference()` | no | `DisplayPreference::Off` if the adapter writes to the raw terminal. The default is `Auto`. |
| `declare_controls(parent)` | no | Declare adapter-level dynamic controls on a subcomponent of the activity's `nmbrs_metrics` component. |
| `shutdown()` | no | Async teardown when a shared adapter is released. |
| `accessor_payload()` | no | A type-erased handle that kernel nodes can look up through the resource scope. |

If `map_op` returns `Err`, construction stops before any cycle runs. If a
dispenser binds fields through a typed API, and not by plain text
substitution, it should build a `Binder` and verify it against `parent`
(`adapter::verify_binders`) inside `map_op`. `Kernel`, `Binder`,
`BinderSlot`, `PortType` and `ExecCtx` are re-exported from
`nmbrs_runtime::adapter`, so an adapter crate doesn't need a direct `polydat`
dependency.

### `OpDispenser`

| Method | Required | Purpose |
|--------|----------|---------|
| `execute(&self, cycle, ctx: &ExecCtx) -> Pin<Box<dyn Future<Output = Result<OpResult, ExecutionError>> + Send>>` | yes | Run one op. |
| `canonical_kernel()` | no | Return `Some(&kernel)` when the dispenser keeps the Polydat kernel it got from `map_op`. The executor builds per-fiber kernels from it. |
| `describe()` / `describe_resolved(wires)` | no | One-line views of the op, as a template and as sent. Used in error diagnostics. |
| `adapter_metrics()` | no | Extra `(family, Labels, MetricValue)` samples to include in metrics snapshots. |
| `status_counters()` | no | Cumulative `(name, count)` pairs for the status line. A leading `_` marks a name as internal. |
| `rows_per_op()` | no | The cursor stride for batch ops. The default is 1. |
| `inner_dispenser()` | no | Wrappers must return their inner dispenser. Leaves keep the default `None`. |

At cycle time, `ctx.wires` (a `wires::WireSource`) is how a dispenser reads
bound values. It resolves names against the fiber's kernel for this
dispenser. `wires::resolve_op_fields_via_wires` renders a list of op fields:

- A field that is exactly `{name}` keeps its typed value.
- References embedded in text are substituted.
- Bare strings stay literal.

The result is a `ResolvedFields`, which has `names`, typed `values`,
`get_str`, `get_value` and `strings()`.

Results and errors:

- **Success** is an `OpResult`. Its `body` is an `Option<Box<dyn ResultBody>>`.
  `TextBody` and `JsonBody` are provided. Implement `ResultBody` for native
  result types; `to_json`, `element_count` and `byte_count` are used for
  captures and traversal metrics.
- **Failure** is an `ExecutionError`:
  - `ExecutionError::Op(AdapterError)` is a failure of this op.
  - `ExecutionError::Adapter(AdapterError)` means the connection or session
    is degraded.
  - `AdapterError` has three fields. `error_name` is the name that the
    workload's `errors:` rules match against. `message` is shown to the
    user. `retryable: true` marks an `Op` error that the `tries:` wrapper
    may re-run.

### Registration

Adapters register at link time with
[`inventory`](https://crates.io/crates/inventory), by submitting an
`AdapterRegistration`:

- `names` are the names users select with `adapter=<name>`.
- `known_params` are adapter params, used for CLI validation.
- `display_preference` decides TUI compatibility from the params.
- `supported_controls` is a list of `control_catalog::ControlDesc`
  descriptors.
- `create` is an async factory from params to an `Arc<dyn DriverAdapter>`.

There are two optional registrations:

- `DriverImpl` lets one adapter have several driver implementations. The
  user picks one with a selector param, such as `cqldriver=`.
- `SharedDriverRegistration` lets phases with the same `ResourceKey` share
  one adapter instance through the resource pool.

A minimal adapter that renders its op fields as text:

```rust,ignore
// Cargo.toml: nmbrs-runtime = "0.3", nmbrs-workload = "0.3",
//             inventory = "0.3", serde_json = "1"
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nmbrs_runtime::adapter::{
    AdapterError, AdapterRegistration, DisplayPreference, DriverAdapter, ExecCtx,
    ExecutionError, Kernel, MapOpFuture, OpDispenser, OpResult, TextBody,
};
use nmbrs_runtime::wires::resolve_op_fields_via_wires;
use nmbrs_workload::model::ParsedOp;

struct EchoAdapter;

impl DriverAdapter for EchoAdapter {
    fn name(&self) -> &str {
        "echo"
    }

    fn map_op<'a>(&'a self, template: &'a ParsedOp, parent: Arc<dyn Kernel>) -> MapOpFuture<'a> {
        Box::pin(async move {
            // Init-time: snapshot the op fields once.
            let fields: Vec<(String, serde_json::Value)> =
                template.op.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Ok(Box::new(EchoDispenser { kernel: parent, fields }) as Box<dyn OpDispenser>)
        })
    }
}

struct EchoDispenser {
    kernel: Arc<dyn Kernel>,
    fields: Vec<(String, serde_json::Value)>,
}

impl OpDispenser for EchoDispenser {
    fn canonical_kernel(&self) -> Option<&Arc<dyn Kernel>> {
        Some(&self.kernel)
    }

    fn execute<'a>(
        &'a self,
        _cycle: u64,
        ctx: &'a ExecCtx<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>> {
        Box::pin(async move {
            let resolved = match resolve_op_fields_via_wires(&self.fields, ctx.wires) {
                Ok(r) => r,
                Err(message) => {
                    return Err(ExecutionError::Op(AdapterError {
                        error_name: "BindError".into(),
                        message,
                        retryable: false,
                    }));
                }
            };
            Ok(OpResult {
                body: Some(Box::new(TextBody(resolved.strings().join(" ")))),
                skipped: false,
            })
        })
    }
}

inventory::submit! {
    AdapterRegistration {
        names: || &["echo"],
        known_params: || &[],
        display_preference: |_params| DisplayPreference::Auto,
        supported_controls: || &[],
        create: |_params| Box::pin(async move {
            Ok(Arc::new(EchoAdapter) as Arc<dyn DriverAdapter>)
        }),
    }
}
```

Reference implementations in the repository:

- [stdout](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs-adapter-stdout/src/lib.rs)
  is the smallest real adapter. It also shows `SharedDriverRegistration`.
- [testkit](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs-adapter-testkit/src/lib.rs)
  injects errors deterministically.
- [http](https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs-adapter-http) and
  [cql](https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs-adapter-cql) are
  full adapters. `cql` uses `DriverImpl` for its `scylla` and
  `cassandra-cpp` drivers.

### Running with your adapter

The published `nmbrs` binary only contains the adapters it was built with. To
use your own adapter, build a binary that links your adapter crate and calls
the runner. Link the crate explicitly (for example with `extern crate`), so
that its `inventory::submit!` registration is kept:

```rust,ignore
extern crate my_adapter; // force-link for inventory registration

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = nmbrs_runtime::runner::run(&args).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
```

```text
my-bench run workload=my_workload.yaml adapter=echo cycles=10
```

`runner::run` accepts `run` or a bare `key=value` / workload-file argument
list. Other `nmbrs` subcommands (`check`, `report`, …) are implemented in
the `nmbrs` crate, not here.

## Execution model

- **Runner** (`runner`). `run(args)` and `run_with_observer(args, observer)`
  run the whole pipeline: parameter handling, workload resolution and
  parsing, adapter creation, scenario execution, metrics and the session.
  `run_executions` runs several executions in one shared session.
  `concurrent::run_workload_headless` runs one workload and returns an
  `ExecutionOutcome`.
- **Scope tree** (`scope_tree`, `scope_kernel`, `scope`, `bindings`).
  Workload, scenario and phase bindings compile into a `ScopeTree` of
  `ScopeKernel`s. Children are bound under their parent's kernel, and op
  templates' kernels are bound under the phase kernel they get in `map_op`.
- **Scenario executor.** It is internal. It walks the `ScenarioNode` tree at
  run time and evaluates `for_each` / `do_while` / `do_until` as it goes.
  `scene_tree::SceneTree` is the view of that walk that renderers use, with
  iterations unrolled into per-iteration phase nodes.
- **Activities and fibers** (`activity`). Each phase runs as an `Activity`,
  configured by `ActivityConfig`:
  - `concurrency` fibers (tokio tasks) execute stanzas.
  - `opseq` maps cycles to ops by ratio (bucket, interval or concat
    sequencing).
  - An optional activity-level `nmbrs_rate::RateLimiter` gates all fibers.
  - Stop conditions, `throttle:` and `tries:` settings apply per activity.
- **Wrappers** (`wrappers`, `wrapper_registry`, `wrapper_resolver`). Op
  fields such as `tries:`, `if:`, `delay:`, `poll:`, `rate:`, `metrics:` and
  `errors:` select wrapper dispensers. These compose around the adapter's
  dispenser in a fixed innermost-to-outermost order (see
  `wrapper_resolver::DEFAULT_ORDER`). Wrappers implement
  `adapter::WrappingDispenser` and expose `inner_dispenser()`.
- **Error routing.** The outermost op wrapper applies the op's
  `nmbrs_errorhandler::ErrorRouter` policy to each terminal failure.
- **Observers** (`observer`). `RunObserver` receives phase and op lifecycle
  events. Implementations:
  - `StderrObserver`, the plain-text default
  - `concurrent::HeadlessObserver`, which collects an outcome with no display
  - `TuiObserver`, provided by `nmbrs-tui`
- **Metrics.** Each activity registers its instruments (`ActivityMetrics`)
  on a component in the `nmbrs_metrics` component tree. Adapter samples from
  `OpDispenser::adapter_metrics` are added to the same snapshots. Dynamic
  controls such as `concurrency` and `rate` are declared on the activity
  component. Each session writes its metrics to a SQLite `metrics.db` in its
  session directory (`session`).

## Cargo features

| Feature | Default | Enables |
|---------|---------|---------|
| `flamegraph` | no | The `profiler=flamegraph` mode: in-process CPU sampling with `pprof`. The SVG is rendered by the external `inferno-flamegraph` tool when it is on `PATH`. Without this feature, `profiler=flamegraph` logs a warning and does nothing. |

The `nmbrs` CLI forwards its own `flamegraph` feature to this one.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-runtime
- Execution engine (SRD 29):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/29_execution_engine.md
- Adapter interface (SRD 30):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/30_adapter_interface.md
- Dispenser-owned Polydat context (SRD 68):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/68_dispenser_owned_polydat_context.md
- Wrappers (SRD 32):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/32_wrappers.md
- Driver resources and sharing (SRD 35):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/35_driver_resources.md

## License

Apache-2.0
