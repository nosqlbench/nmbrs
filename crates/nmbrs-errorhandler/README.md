# nmbrs-errorhandler

A composable error router for the nmbrs op-dispatch loop. Errors are
classified by name, matched against regex rules, and passed through a chain
of handlers that can log, count, mark the error retryable, or stop the run.
This crate provides the `errors:` policy used by nmbrs workloads, and you can
also use it on its own.

## Where it sits in nmbrs

- Used by [`nmbrs-runtime`](https://crates.io/crates/nmbrs-runtime), which
  builds an `ErrorRouter` from each workload's `errors` spec and applies it
  in its outermost op wrapper, and by the
  [`nmbrs`](https://crates.io/crates/nmbrs) CLI.
- Has no nmbrs dependencies. Its only dependencies are `regex` and `log`.

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs) CLI
and never depend on this crate directly. They write the policy as a workload
parameter:

```yaml
# from crates/nmbrs/examples/workloads/controls/error_rate_circuit_breaker.yaml
params:
  adapter: testkit
  # Count every error and keep running; the `stop_when` guard, not the
  # per-op policy, decides when the run is unhealthy enough to fail.
  errors: ".*:warn,counter"
```

([full example](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/controls/error_rate_circuit_breaker.yaml))

When a workload sets no `errors` spec, the nmbrs runner uses `.*:warn,stop`.

## Spec syntax

```text
TimeoutError:retry,warn,counter;.*:stop
```

- Rules are separated by `;`. Each rule is `patterns:handlers`.
- Patterns (left of the first `:`) are regular expressions matched against
  the error name. A rule can list several patterns, separated by `,`.
- Handlers (right of the `:`) are separated by `,` and run in order.
- A rule with no `:` is a handler list that applies to every error (`.*`).
- The first rule whose pattern matches the error name wins. Lookups are
  cached per error name.
- An error name that matches no rule is handled by `stop`, and a message
  goes to stderr. `ErrorRouter::has_catch_all()` reports whether the spec
  contains a literal `.*` rule.

Built-in handlers (`nmbrs_errorhandler::handlers::builtin_handler`):

| Name | Handler | Effect |
|------|---------|--------|
| `stop` | `StopHandler` | Sets `should_stop`. Does not log; use `warn,stop` to do both. |
| `warn` | `WarnHandler` | Logs `WARN error at cycle N: [name] message`. |
| `error` | `ErrorLogHandler` | Logs `ERROR at cycle N: [name] message`. |
| `ignore` | `IgnoreHandler` | No-op pass-through. |
| `retry` | `RetryHandler` | Marks the detail retryable. |
| `counter` / `count` | `CounterHandler` | Counts occurrences per error name. |

`retry` also accepts a budget, `retry(N)`. The router records the largest
budget across its rules, and `ErrorRouter::retry_verb_budget()` returns it
(`Some(3)` for a bare `retry`, or `None` when no rule uses `retry`). nmbrs-runtime uses
this value to give ops a `tries` budget when they don't declare one. A
malformed argument such as `retry(lots)` is a parse error, as is an unknown
handler name.

## API

- `ErrorRouter`:
  - `parse(&str) -> Result<ErrorRouter, String>` builds a router from a spec.
  - `handle_error(name, msg, cycle, duration_nanos) -> ErrorDetail` runs the
    matching handler chain.
  - `default_stop()` is the same as `.*:stop`.
  - `default_warn_count()` is the same as `.*:warn,counter`.
- `ErrorDetail` is the value passed through the chain. It has four public
  fields (`name`, `retry: Retry`, `result_code`, `should_stop`) and builder
  methods (`with_retryable`, `with_not_retryable`, `with_result_code`,
  `with_stop`). `handle_error` starts each chain from
  `ErrorDetail::non_retryable(name)`, whose `result_code` is `127`.
- `ErrorHandler` is the trait every handler implements:
  `handle(&self, name, error_msg, cycle, duration_nanos, detail) -> ErrorDetail`.
  `ErrorRouter::parse` only resolves the built-in names above. A custom
  `ErrorHandler` can be called directly, but it can't be named in a spec.
- `handlers::set_log_fn(fn(&str))` redirects what `warn` and `error` log.
  They write to stderr by default. nmbrs-runtime uses this to send them to
  its own log.

```rust
use nmbrs_errorhandler::{ErrorDetail, ErrorRouter};

let router = ErrorRouter::parse("Timeout.*:retry,counter;.*:warn,stop").unwrap();

let d = router.handle_error("TimeoutError", "timed out", 42, 1_000_000);
assert!(d.is_retryable());
assert!(!d.should_stop);

let d = router.handle_error("BadCredentials", "auth failed", 43, 0);
assert!(d.should_stop);

assert_eq!(router.retry_verb_budget(), Some(3));
assert!(router.has_catch_all());

let custom = ErrorDetail::retryable("Overloaded").with_result_code(503);
assert_eq!(custom.result_code, 503);
```

## Cargo features

None.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-errorhandler
- Design (SRD 07, error routing):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/07_error_routing.md

## License

Apache-2.0
