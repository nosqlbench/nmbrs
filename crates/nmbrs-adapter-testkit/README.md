# nmbrs-adapter-testkit

The `testkit` adapter for [nmbrs](https://crates.io/crates/nmbrs). It runs ops
against a simulated backend. Each op is rendered and printed the same way the
`stdout` adapter does it. Then per-op settings add a synthetic result body,
latency, a capacity limit, overload rejection, injected errors or injected
panics. This lets you build and test workloads, error handling, retries and
control loops without a live service.

## Using it

This adapter is used through the `nmbrs` CLI. Select it with `adapter=testkit`,
either on the command line or in the workload (`params:`, a phase, or a single
op).

### Adapter parameters

| Param | Default | Effect |
|-------|---------|--------|
| `filename` | `stdout` | Where the rendered op text goes. |
| `format` | `stmt` | How ops are rendered. Takes the same names as the `stdout` adapter: `stmt`, `readout`, `assignments`, `json`, `csv`, `tsv`, `raw`. |

### Op fields

Put these fields on the op, either as keys of the op itself or inside the op's
`params:` block. If both have the same key, `params:` wins. Except for
`result-load`, they are read once, as literal values, when the op is mapped.

| Field | Default | Effect |
|-------|---------|--------|
| `result-body` | none | Any YAML/JSON value (map, list or scalar). It becomes the op's JSON result body, so `capture:`, `result:` and `verify:` can read it like a real response. Without it, the result body is the rendered op text. |
| `result-latency` | none | Simulated service time: `"5ms"`, `"200us"`, or a bare number of milliseconds. |
| `result-capacity` | unlimited | At most N ops of this template are serviced at once. The rest wait, and the wait counts toward their latency. `0` means unlimited. |
| `result-overload` | none | Reject the op with a retryable `Overload` error when the number of in-flight ops (waiting plus serving) is greater than N. `0` disables it. |
| `result-load` | none | Resolved on every cycle, and bind points are allowed. When set, `result-overload` is compared against this value instead of the measured in-flight count, so overload depends only on the configured load. |
| `result-error-rate` | `0` | Fraction of cycles (0.0 to 1.0) that fail. The choice of cycle is deterministic: a hash of the cycle number. |
| `result-error-name` | `ModelError` | Error name for injected errors, which is what `errors:` policies match on. |
| `result-error-message` | `simulated error` | Error message for injected errors. |
| `result-throw-at` | none | Fail with a non-retryable error on the cycle equal to this number. |
| `result-throw-name` | `ThrowAt` | Error name for `result-throw-at`. |
| `result-panic-rate` | `0` | Fraction of cycles on which the op panics instead of returning an error. Uses a different hash stream from `result-error-rate`, so the two pick different cycles. |
| `result-panic-message` | `testkit: injected op panic` | Panic message. The cycle number is appended. |

Every op passes through these steps in this order: render and print, overload
check, wait for capacity, `result-throw-at`, error injection, panic injection,
latency, result.

Injected errors from `result-error-rate`, from
[`controls/error_rate_circuit_breaker.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/controls/error_rate_circuit_breaker.yaml):

```yaml
params:
  adapter: testkit
  rate: "2000"
  concurrency: "8"
  errors: ".*:warn,counter"

phases:
  steady:
    cycles: 3000
    rate: "{rate}"
    concurrency: "{concurrency}"
    ops:
      query:
        stmt: "SELECT value FROM t WHERE id = {cycle};"
        result-error-rate: 0.001
        result-error-name: Timeout
        result-error-message: "synthetic read timeout (counted; under budget)"
```

A backend that can hold only a few requests at once, from
[`controls/throttle_backpressure.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/controls/throttle_backpressure.yaml):

```yaml
    ops:
      pressured:
        stmt: "x"
        tries: 8
        retry_backoff: "10ms"
        result-overload: 4
        result-latency: "5ms"
```

A synthetic result body read by captures, from
[`controls/phase_poll_smoke.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/controls/phase_poll_smoke.yaml):

```yaml
    ops:
      read_state:
        result-body:
          - value: 1
          - value: []
          - value: 0
        capture:
          sstables:       "/0/value"
          active_for_cf:  "/1/value:count"
          pending_for_cf: "/2/value"
```

### Polydat test functions

When this crate is linked, it also registers some Polydat functions. They exist
to test resume and failure handling, and ordinary workloads should not rely on
them:

- `testkit_throw_at(value, threshold, errorname)` passes `value` through and
  panics with `errorname` when `value == threshold`.
- `testkit_side_effect_sequence_next_cycling(statefile_path, csv_values)` and
  `testkit_side_effect_sequence_next_noncycling(statefile_path, csv_values)`
  return the next value from a comma-separated list once per session, and keep
  their position in a state file.
- `testkit_side_effect_sequence_reset(statefile_path)` deletes that state file.

## Cargo features

None.

## Where it sits

- Implements `DriverAdapter` and `OpDispenser` from
  [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime) and registers itself
  under the name `testkit` via `inventory`.
- Reads op templates from
  [nmbrs-workload](https://crates.io/crates/nmbrs-workload), and uses
  [nmbrs-adapter-stdout](https://crates.io/crates/nmbrs-adapter-stdout) for op
  rendering.
- For Rust callers, the crate exports `ModelAdapter`, `ModelConfig`,
  `ModelParams`, `ResultDef` and `extract_model_params`, plus the
  `polydat_fixtures` module.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-adapter-testkit
- nmbrs CLI: https://crates.io/crates/nmbrs

## License

Apache-2.0
