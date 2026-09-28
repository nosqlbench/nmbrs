# nmbrs-rate

An async token-bucket rate limiter built on `tokio::sync::Semaphore`. It uses
time-scaled permits and burst recovery. nmbrs uses it to cap op throughput,
and it reports how long callers were held back so that coordinated omission
is visible in the results. You can retarget it while it runs.

## Where it sits in nmbrs

- Used by [`nmbrs-runtime`](https://crates.io/crates/nmbrs-runtime):
  - Each activity (phase) with a `rate` gets one `RateLimiter`. The limiter
    is registered as the target of that activity's `rate` dynamic control.
  - An op-level `rate:` field adds a per-op limiter through the runtime's
    `rate` wrapper.
- Also used by the [`nmbrs`](https://crates.io/crates/nmbrs) CLI.
- Depends on [`nmbrs-metrics`](https://crates.io/crates/nmbrs-metrics) for the
  `ControlApplier` trait, and on `tokio` and `futures`.

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs) CLI
and set rates in a workload or on the command line (`rate=1000`):

```yaml
# excerpt from crates/nmbrs/examples/workloads/controls/error_rate_circuit_breaker.yaml
params:
  rate: "2000"
  concurrency: "8"

phases:
  steady:
    cycles: 3000
    rate: "{rate}"
    concurrency: "{concurrency}"
```

([full example](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/controls/error_rate_circuit_breaker.yaml))

## What it provides

- **`RateSpec`** is the limiter configuration. Its public fields are
  `ops_per_sec`, `burst_ratio`, `verb: Verb` and `unit: TimeUnit`.
  - `RateSpec::new(ops)` uses a burst ratio of 1.1.
  - `RateSpec::with_burst(ops, ratio)` sets the burst ratio.
  - `RateSpec::parse(&str)` reads the comma-separated form used by params and
    CLI flags:

    ```text
    1000              # 1000 ops/s, burst 1.1, verb start
    1000,1.5          # 1000 ops/s, burst 1.5
    1000,1.1,restart  # with a verb: start | configure | restart | stop
    ```

    A rate that isn't positive, or an unknown verb, is an error.
- **`TimeUnit`** is the tick unit (`Nanos`, `Micros`, `Millis`, `Seconds`).
  `TimeUnit::for_rate` chooses it from the target rate so that ticks per op
  fit in a `u32`.
- **`RateLimiter`**:
  - `RateLimiter::start(spec)` spawns a refill task on the current tokio
    runtime, so it must be called from inside one. The task tops up permits
    every 10 ms. Ticks that overflow the active pool go to a waiting pool.
    Burst recovery moves ticks back from the waiting pool, up to
    `burst_ratio`.
  - `acquire().await` waits for one op's worth of permits and returns the
    current backlog in ticks.
  - `wait_time_nanos()` returns that backlog in nanoseconds. This is the
    coordinated-omission signal.
  - `total_blocks()` counts `acquire` calls. `rate()` and `spec()` report
    the current configuration.
  - `stop().await` ends the refill task. Dropping the limiter also stops it.
  - `reconfigure(spec)` swaps the rate, burst ratio and unit in place,
    without restarting the refill task. The next `acquire` uses the new
    cost, and the existing backlog is kept. It rejects `ops_per_sec <= 0`
    and `burst_ratio < 1.0`.
- **`RateLimiterApplier`** implements
  `nmbrs_metrics::controls::ControlApplier<RateSpec>`. Register it on a
  `Control<RateSpec>`, and every successful `set` on that control calls
  `reconfigure` on the limiter.

```rust,no_run
use nmbrs_rate::{RateLimiter, RateSpec};

// Must run inside a tokio runtime: `start` spawns the refill task.
async fn run() {
    // Target 1000 ops/s with the default burst ratio.
    let limiter = RateLimiter::start(RateSpec::parse("1000").unwrap());

    for _ in 0..10_000 {
        let _backlog_ticks = limiter.acquire().await;
        // ... issue the op ...
    }
    println!("behind schedule by {} ns", limiter.wait_time_nanos());

    // Raise the ceiling tenfold without stopping.
    limiter.reconfigure(RateSpec::new(10_000.0)).unwrap();
    limiter.stop().await;
}
```

Driving the limiter through a dynamic control (requires `nmbrs-metrics`):

```rust,ignore
use std::sync::Arc;
use nmbrs_metrics::controls::{Control, ControlBuilder, ControlOrigin};
use nmbrs_rate::{RateLimiter, RateLimiterApplier, RateSpec};

let limiter = Arc::new(RateLimiter::start(RateSpec::new(100.0)));
let control: Control<RateSpec> = ControlBuilder::new("rate", RateSpec::new(100.0)).build();
control.register_applier(RateLimiterApplier::new(limiter.clone()));

control.set(RateSpec::new(5_000.0), ControlOrigin::Cli).await?;
assert_eq!(limiter.rate(), 5_000.0);
```

## Cargo features

None.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-rate
- Design (SRD 06, rate limiter):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/06_rate_limiter.md
- Design notes and coordinated-omission rationale:
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/notes/19_rate_limiter.md

## License

Apache-2.0
