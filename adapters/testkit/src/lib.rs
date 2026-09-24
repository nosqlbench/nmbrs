// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Model adapter: simulates operation execution for workload prototyping.
//!
//! Extends the stdout adapter with:
//! - Synthetic structured results via the `result-body` op field (any
//!   JSON shape — map, array, scalar), emitted as a [`JsonBody`] so
//!   captures and `verify:` address it like a real backend response
//! - Latency simulation via `result-latency`
//! - Deterministic error injection via `result-error-rate`
//! - Backend saturation simulation via `result-capacity` / `result-overload`
//! - Diagnostic output via `--diagnose`
//!
//! When no `result-body` field is present, behaves identically to stdout.
//! See SRD 29 for the full design.
//!
//! ## Oversaturation modeling
//!
//! Two op-level knobs let a workload simulate a capacity-limited
//! backend so operators can *see* the effect of pushing concurrency
//! past what the simulated server can absorb:
//!
//! * `result-capacity = N` — soft cap implemented as a
//!   [`tokio::sync::Semaphore`] with `N` permits. Ops acquire a
//!   permit before the latency sleep and release on completion, so
//!   up to `N` ops are serviced concurrently; the rest queue. As the
//!   caller's `concurrency` control climbs past `N`, the per-op
//!   wait time climbs with it (classic M/M/c queueing behavior)
//!   while throughput tops out at `N / result-latency`.
//!
//! * `result-overload = M` — hard threshold. If the number of
//!   in-flight ops (after acquiring a permit) exceeds `M`, the op is
//!   rejected with an `Overload` error instead of being serviced.
//!   Use alongside `capacity` to model a backend that queues up to a
//!   point and then starts returning errors — which is what the
//!   `dynamic_controls` example's feedback loop watches for so it
//!   can throttle the rate back.
//!
//! Both are per-op: different ops simulate independent backends.

pub mod polydat_fixtures;

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

use nmbrs_adapter_stdout::{StdoutConfig, StdoutFormat};
use nmbrs_runtime::adapter::{
    AdapterError, DriverAdapter, ExecutionError, JsonBody, OpDispenser, OpResult, TextBody,
};
use nmbrs_workload::model::ParsedOp;
use xxhash_rust::xxh3;

/// Distinct xxh3 seed for the `panic_rate` injection stream, so injected
/// panics fall on a cycle set independent of injected errors (which hash the
/// bare cycle with the default seed).
const PANIC_SEED: u64 = 0x5041_4e49_435f_5f5f; // "PANIC___"

/// Configuration for the model adapter.
#[derive(Default)]
pub struct ModelConfig {
    /// Base stdout config (filename, newline, format).
    pub stdout: StdoutConfig,
    /// Whether to print diagnostic output (--diagnose).
    pub diagnose: bool,
}

/// Simulated result definition for a single op.
///
/// Attached to ops via the `result-body` field in the workload YAML.
/// When present, the model adapter wraps it in a [`JsonBody`] so
/// capture path-expressions (`capture: { x: "/0/value" }`) and
/// `verify:` field assertions address it exactly as they would a
/// real backend's structured response — letting a workload satisfy
/// a phase-poll `until:` predicate from synthetic state, no live
/// service required.
#[derive(Debug, Clone)]
pub enum ResultDef {
    /// A literal synthetic result body of any JSON shape — a map
    /// (`result-body: { user_id: 42 }`), an array
    /// (`result-body: [ { value: 1 }, { value: 0 } ]`), or a scalar.
    /// Emitted verbatim as the op's [`JsonBody`].
    Json(serde_json::Value),
}

/// Per-op model parameters extracted from `result-*` op fields.
#[derive(Debug, Clone)]
pub struct ModelParams {
    /// Simulated result definition (None = no result, behave like stdout).
    pub result: Option<ResultDef>,
    /// Simulated latency in milliseconds (None = instant).
    pub latency_ms: Option<f64>,
    /// Error injection rate (0.0-1.0). Deterministic per cycle.
    pub error_rate: f64,
    /// Error class name for the error router.
    pub error_name: String,
    /// Error detail message.
    pub error_message: String,
    /// Simulated backend concurrency cap. `Some(n)` installs an n-permit
    /// [`Semaphore`] the op must acquire before its latency sleep, so ops
    /// above `n` queue — throughput caps at `n / latency_ms` and per-op
    /// latency grows with caller concurrency. `None` = unlimited.
    pub capacity: Option<usize>,
    /// In-flight threshold above which the op is rejected with an
    /// `Overload` error instead of being serviced. Checked *after* a
    /// permit is acquired, so this represents "too many already
    /// being served." `None` = no overload rejection.
    pub overload: Option<usize>,
    /// Driver-level fail-on-cycle test fixture (SRD-44 §"resumable
    /// test fixture"; design memo `resumable_test_fixture.md`
    /// variant A). When set, the op returns an `Err` tagged
    /// `result-throw-name` (default `ThrowAt`) on the cycle whose
    /// numeric value equals this threshold. Distinct surface from
    /// the GK-level `testkit_throw_at(...)` node — this throws from inside
    /// the adapter's op execution path, exercising the
    /// op-result-error branch of the errors cascade.
    pub throw_at_cycle: Option<u64>,
    /// Error name to tag the synthetic throw with. Default
    /// `ThrowAt`; pin a specific name when the workload's
    /// errors cascade needs to match a label.
    pub throw_name: String,
    /// Panic injection rate (0.0–1.0), deterministic per cycle on a stream
    /// INDEPENDENT of `error_rate` (distinct seed), so injected panics and
    /// injected errors don't land on the same cycles. When it fires the op
    /// PANICS — unwinds out of `execute` rather than returning `Err` — the
    /// misbehaviour that exercises the fiber-boundary panic catch keeping a
    /// phase's target concurrency intact irrespective of error handling.
    /// `0.0` = never.
    pub panic_rate: f64,
    /// Message carried by an injected panic (the cycle is appended for a
    /// reproducible signature). Default `testkit: injected op panic`.
    pub panic_message: String,
}

impl Default for ModelParams {
    fn default() -> Self {
        Self {
            result: None,
            latency_ms: None,
            error_rate: 0.0,
            error_name: "ModelError".into(),
            error_message: "simulated error".into(),
            capacity: None,
            overload: None,
            throw_at_cycle: None,
            throw_name: "ThrowAt".into(),
            panic_rate: 0.0,
            panic_message: "testkit: injected op panic".into(),
        }
    }
}

/// Output target (same pattern as stdout adapter).
enum OutputTarget {
    Stdout(BufWriter<io::Stdout>),
    File(BufWriter<std::fs::File>),
}

impl Write for OutputTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            OutputTarget::Stdout(w) => w.write(buf),
            OutputTarget::File(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            OutputTarget::Stdout(w) => w.flush(),
            OutputTarget::File(w) => w.flush(),
        }
    }
}

/// The model adapter: stdout + simulated results, latency, and errors.
///
/// Prints the resolved op (like stdout), then produces a simulated
/// result based on the op's `result` field configuration. Supports
/// latency injection and deterministic error simulation.
///
/// Use for prototyping workloads before connecting to real infrastructure.
pub struct ModelAdapter {
    writer: Arc<Mutex<OutputTarget>>,
    newline: bool,
    format: StdoutFormat,
    diagnose: bool,
}

impl Default for ModelAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelAdapter {
    /// Create with default config.
    pub fn new() -> Self {
        Self::with_config(ModelConfig::default())
    }

    /// Create with explicit config.
    pub fn with_config(config: ModelConfig) -> Self {
        let writer = if config.stdout.filename.eq_ignore_ascii_case("stdout") {
            OutputTarget::Stdout(BufWriter::new(io::stdout()))
        } else {
            let file = std::fs::File::create(&config.stdout.filename).unwrap_or_else(|e| {
                panic!(
                    "failed to create output file '{}': {e}",
                    config.stdout.filename
                )
            });
            OutputTarget::File(BufWriter::new(file))
        };
        Self {
            writer: Arc::new(Mutex::new(writer)),
            newline: config.stdout.newline,
            format: config.stdout.format,
            diagnose: config.diagnose,
        }
    }
}

impl DriverAdapter for ModelAdapter {
    fn name(&self) -> &str {
        "testkit"
    }

    fn map_op<'a>(
        &'a self,
        template: &'a ParsedOp,
        parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // The yaml parser routes unknown top-level op keys into
            // `template.op`, while a nested `params:` block lands in
            // `template.params`. Both are valid ways to declare
            // `result-*` fields, so merge them — `params` wins on
            // collision because an explicit `params:` block is the
            // stronger user intent.
            let mut merged = template.op.clone();
            for (k, v) in &template.params {
                merged.insert(k.clone(), v.clone());
            }
            let model_params = extract_model_params(&merged);
            // Per-op semaphore: independent ops simulate independent
            // backends, each with their own capacity ceiling.
            let semaphore = model_params.capacity.map(|n| Arc::new(Semaphore::new(n)));
            // SRD-68 Push 5: snapshot op-field templates for cycle-time
            // resolution through the generic wires API.
            let op_fields: Vec<(String, serde_json::Value)> = template
                .op
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Ok(Box::new(ModelDispenser {
                writer: self.writer.clone(),
                format: self.format,
                newline: self.newline,
                diagnose: self.diagnose,
                model_params,
                semaphore,
                in_flight: Arc::new(AtomicUsize::new(0)),
                canonical_kernel: parent,
                op_fields,
            }) as Box<dyn OpDispenser>)
        })
    }
}

/// Op dispenser for the model adapter. Captures format and model params
/// at init time; renders and simulates per-cycle.
struct ModelDispenser {
    writer: Arc<Mutex<OutputTarget>>,
    format: StdoutFormat,
    newline: bool,
    diagnose: bool,
    model_params: ModelParams,
    /// Simulated-backend permit pool. `Some` when `result-capacity` is
    /// set; held for the full service time so waiting ops observe
    /// queueing delay. `None` = no capacity limit.
    semaphore: Option<Arc<Semaphore>>,
    /// Count of ops currently being serviced (after permit acquire,
    /// before latency-sleep completion). Used for the overload check
    /// and diagnostic output.
    in_flight: Arc<AtomicUsize>,
    /// SRD-68 invariant I-3: dispenser-owned canonical Polydat Kernel.
    canonical_kernel: std::sync::Arc<polydat::kernel::PolydatKernel>,
    /// Op-field templates snapshotted at `map_op`. Resolved per
    /// cycle via the generic `wires` API; the rendered text feeds
    /// the trace writer and the OpResult body.
    op_fields: Vec<(String, serde_json::Value)>,
}

/// RAII guard that decrements the in-flight counter on drop, so the
/// count stays accurate across every exit path (success, error
/// injection, overload rejection, panic).
struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl OpDispenser for ModelDispenser {
    fn canonical_kernel(&self) -> Option<&std::sync::Arc<polydat::kernel::PolydatKernel>> {
        Some(&self.canonical_kernel)
    }

    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        Box::pin(async move {
            let resolved =
                nmbrs_runtime::wires::resolve_op_fields_via_wires(&self.op_fields, wires).map_err(
                    |msg| {
                        ExecutionError::Op(AdapterError {
                            error_name: "BindError".into(),
                            message: msg,
                            retryable: false,
                        })
                    },
                )?;
            let text = self.format.render(&resolved, ",");

            // Write the resolved op (same as stdout). Done before
            // any saturation simulation so the trace reflects the
            // op the caller issued, regardless of how it resolved.
            // A real-stdout target routes through the observer's op-output
            // channel (coordinated with a live status display; straight to stdout
            // when piped) so it doesn't staircase under a raw-mode terminal; a
            // file target writes directly.
            {
                let to_stdout = matches!(
                    &*self.writer.lock().unwrap_or_else(|e| e.into_inner()),
                    OutputTarget::Stdout(_)
                );
                if to_stdout {
                    nmbrs_runtime::observer::op_output(&text);
                } else {
                    let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                    let write_result = if self.newline {
                        writeln!(writer, "{text}")
                    } else {
                        write!(writer, "{text}")
                    };
                    if let Err(e) = write_result {
                        return Err(ExecutionError::Op(AdapterError {
                            error_name: "IoError".into(),
                            message: format!("write failed: {e}"),
                            retryable: false,
                        }));
                    }
                    if let Err(e) = writer.flush() {
                        return Err(ExecutionError::Op(AdapterError {
                            error_name: "FlushError".into(),
                            message: format!("flush failed: {e}"),
                            retryable: false,
                        }));
                    }
                }
            }

            // Track occupancy (waiting + serving) BEFORE taking a
            // permit so the measured-overload model can see the real
            // pressure on the simulated backend — including ops queued,
            // not just being serviced.
            let current = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            let _guard = InFlightGuard(self.in_flight.clone());

            // Overload rejection. Two models, picked per op:
            //
            //  - SYNTHETIC (preferred for reproducible objectives): when
            //    the op declares `result-load` (a *logical* load level —
            //    typically the searched `{conc}` or a predicted in-flight),
            //    overload is decided from THAT value, not the measured
            //    in-flight count. The signal is then a deterministic
            //    function of the setting: a saturating setting overloads on
            //    EVERY op regardless of host throughput, so an optimizer's
            //    `rate(errors_total[…])` is reliably > 0 there and 0 at a
            //    safe setting — identical serial or under concurrent
            //    pressure (no actual host saturation required).
            //  - MEASURED (default): overload when the real in-flight count
            //    exceeds the threshold — for throughput/saturation probing
            //    (e.g. capacity_probe) where the measured pressure IS the
            //    point.
            //
            // Either way the rejection is retryable, fails fast (before a
            // permit), and never consumes service capacity.
            let synthetic_load = resolved_field_f64(&resolved, "result-load");
            let overload_hit = match (synthetic_load, self.model_params.overload) {
                (Some(load), Some(threshold)) => load > threshold as f64,
                (None, Some(threshold)) => current > threshold,
                _ => false,
            };
            if overload_hit {
                let threshold = self.model_params.overload.unwrap_or(0);
                let detail = match synthetic_load {
                    Some(load) => format!("load={load}"),
                    None => format!("in_flight={current}"),
                };
                if self.diagnose {
                    // SRD-87 A1: diagnostics route through the log channel.
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Info,
                        "testkit: OVERLOAD cycle={cycle} {detail} threshold={threshold}"
                    );
                }
                return Err(ExecutionError::Op(AdapterError {
                    error_name: "Overload".into(),
                    message: format!("simulated overload: {detail} > {threshold}"),
                    retryable: true,
                }));
            }

            // Simulated-backend admission. Any await here counts
            // toward the caller's observed latency, which is the
            // point — queueing delay is what the caller sees when
            // the backend is oversaturated.
            let _permit = match &self.semaphore {
                Some(sem) => Some(sem.clone().acquire_owned().await.map_err(|e| {
                    ExecutionError::Op(AdapterError {
                        error_name: "SemaphoreClosed".into(),
                        message: format!("testkit semaphore closed: {e}"),
                        retryable: false,
                    })
                })?),
                None => None,
            };

            // Driver-level `result-throw-at` fixture (resumable-test
            // staircase). Tripping here surfaces the failure at
            // the op-result boundary, exercising the
            // `Result<OpResult, ExecutionError>` branch of the
            // errors cascade — distinct from the GK-level
            // `testkit_throw_at(...)` node which panics during binding
            // eval.
            if let Some(threshold) = self.model_params.throw_at_cycle
                && cycle == threshold
            {
                if self.diagnose {
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Info,
                        "testkit: ThrowAt cycle={cycle} threshold={threshold}"
                    );
                }
                return Err(ExecutionError::Op(AdapterError {
                    error_name: self.model_params.throw_name.clone(),
                    message: format!(
                        "testkit driver-level result-throw-at: cycle {cycle} reached threshold {threshold}",
                    ),
                    retryable: false,
                }));
            }

            // Error injection: deterministic per cycle
            if self.model_params.error_rate > 0.0 {
                let h = xxh3::xxh3_64(&cycle.to_le_bytes());
                let p = h as f64 / u64::MAX as f64;
                if p < self.model_params.error_rate {
                    if self.diagnose {
                        nmbrs_runtime::diag!(
                            nmbrs_runtime::observer::LogLevel::Info,
                            "model: ERROR injected (cycle={}, rate={:.2}%)",
                            cycle,
                            self.model_params.error_rate * 100.0
                        );
                    }
                    return Err(ExecutionError::Op(AdapterError {
                        error_name: self.model_params.error_name.clone(),
                        message: self.model_params.error_message.clone(),
                        retryable: false,
                    }));
                }
            }

            // Panic injection: deterministic per cycle on a stream
            // INDEPENDENT of `error_rate` (distinct seed), so injected panics
            // and injected errors don't collide on the same cycles. Unlike
            // every other misbehaviour this UNWINDS out of `execute` — the
            // adapter-panic misbehaviour that the fiber's op-boundary
            // `catch_unwind` must survive without dropping the fiber
            // (concurrency invariant). See `op_panic_resilience.yaml`.
            if self.model_params.panic_rate > 0.0 {
                let h = xxh3::xxh3_64_with_seed(&cycle.to_le_bytes(), PANIC_SEED);
                let p = h as f64 / u64::MAX as f64;
                if p < self.model_params.panic_rate {
                    panic!("{} (cycle={cycle})", self.model_params.panic_message);
                }
            }

            // Service time. Happens *while holding the permit*, so
            // later ops wait in the semaphore queue rather than
            // racing through.
            if let Some(ms) = self.model_params.latency_ms
                && ms > 0.0
            {
                let duration = std::time::Duration::from_micros((ms * 1000.0) as u64);
                tokio::time::sleep(duration).await;
            }

            // A declared `result:` becomes the op's structured body
            // (so captures / `verify:` can address it); otherwise the
            // body is the rendered op text, exactly like stdout.
            let body: Box<dyn nmbrs_runtime::adapter::ResultBody> = match &self.model_params.result
            {
                Some(ResultDef::Json(v)) => Box::new(JsonBody(v.clone())),
                None => Box::new(TextBody(text)),
            };
            Ok(OpResult {
                body: Some(body),
                skipped: false,
            })
        })
    }
}

/// A per-cycle resolved op field as f64, or `None` if absent / non-numeric.
/// Used to read the synthetic `result-load` (resolved through the wires each
/// cycle, so it tracks the live searched coordinate, e.g. `{conc}`).
fn resolved_field_f64(
    resolved: &nmbrs_runtime::adapter::ResolvedFields,
    name: &str,
) -> Option<f64> {
    let idx = resolved.names.iter().position(|n| n == name)?;
    match resolved.values.get(idx)? {
        polydat::ast::Value::F64(f) => Some(*f),
        polydat::ast::Value::U64(u) => Some(*u as f64),
        polydat::ast::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        polydat::ast::Value::Str(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Extract model parameters from an op's params/fields.
///
/// Looks for `result-body`, `result-latency`, `result-error-rate`,
/// `result-error-name`, `result-error-message` in the op's params.
pub fn extract_model_params(params: &HashMap<String, serde_json::Value>) -> ModelParams {
    let mut mp = ModelParams::default();

    // `result-body:` — any JSON shape (map / array / scalar), emitted
    // verbatim as the op's structured body so captures and `verify:`
    // can address it. (Not `result:` — that key is the reserved
    // SRD-66 result-bindings block, consumed by the workload model
    // before adapter params are read.) `null` means "no synthetic
    // body" (behave like stdout), distinct from absent.
    if let Some(val) = params.get("result-body")
        && !val.is_null()
    {
        mp.result = Some(ResultDef::Json(val.clone()));
    }

    if let Some(val) = params.get("result-latency") {
        if let Some(s) = val.as_str() {
            mp.latency_ms = parse_latency(s);
        } else if let Some(n) = val.as_f64() {
            mp.latency_ms = Some(n);
        }
    }

    if let Some(val) = params.get("result-error-rate")
        && let Some(n) = val.as_f64()
    {
        mp.error_rate = n;
    }

    if let Some(val) = params.get("result-error-name")
        && let Some(s) = val.as_str()
    {
        mp.error_name = s.to_string();
    }

    if let Some(val) = params.get("result-error-message")
        && let Some(s) = val.as_str()
    {
        mp.error_message = s.to_string();
    }

    // result-panic-rate: deterministic per-cycle PANIC injection (the op
    // unwinds out of `execute` instead of returning). Accepts a JSON number
    // or a numeric string (workloads often interpolate a binding here).
    if let Some(val) = params.get("result-panic-rate") {
        if let Some(n) = val.as_f64() {
            mp.panic_rate = n;
        } else if let Some(n) = val.as_str().and_then(|s| s.trim().parse::<f64>().ok()) {
            mp.panic_rate = n;
        }
    }

    if let Some(val) = params.get("result-panic-message")
        && let Some(s) = val.as_str()
    {
        mp.panic_message = s.to_string();
    }

    if let Some(n) = params.get("result-capacity").and_then(parse_usize_param)
        && n > 0
    {
        mp.capacity = Some(n);
    }

    if let Some(n) = params.get("result-overload").and_then(parse_usize_param)
        && n > 0
    {
        mp.overload = Some(n);
    }

    // result-throw-at: fail on the cycle whose value equals this
    // threshold. Accepts either a JSON number or a numeric string
    // (workload YAML often interpolates a binding here, which
    // arrives as a string).
    if let Some(val) = params.get("result-throw-at") {
        if let Some(n) = val.as_u64() {
            mp.throw_at_cycle = Some(n);
        } else if let Some(s) = val.as_str()
            && let Ok(n) = s.trim().parse::<u64>()
        {
            mp.throw_at_cycle = Some(n);
        }
    }
    if let Some(val) = params.get("result-throw-name")
        && let Some(s) = val.as_str()
    {
        mp.throw_name = s.to_string();
    }

    mp
}

/// Parse a positive integer from a JSON value, accepting either a
/// native number or a numeric string (so YAML `"4"` and `4` both work).
fn parse_usize_param(v: &serde_json::Value) -> Option<usize> {
    if let Some(n) = v.as_u64() {
        return usize::try_from(n).ok();
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse().ok();
    }
    None
}

/// Parse a latency string like "5ms", "200us", or just a number (ms).
fn parse_latency(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        n.trim().parse().ok()
    } else if let Some(n) = s.strip_suffix("us") {
        n.trim().parse::<f64>().ok().map(|v| v / 1000.0)
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_runtime::adapter::ResolvedFields;

    /// Minimal kernel used as the `parent` argument to `map_op`
    /// in tests that don't need a richer Polydat context (SRD-68 Push 2).
    fn test_kernel() -> std::sync::Arc<polydat::kernel::PolydatKernel> {
        std::sync::Arc::new(
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap(),
        )
    }

    #[test]
    fn parse_latency_ms() {
        assert_eq!(parse_latency("5ms"), Some(5.0));
        assert_eq!(parse_latency("  10ms  "), Some(10.0));
    }

    #[test]
    fn parse_latency_us() {
        assert_eq!(parse_latency("500us"), Some(0.5));
    }

    #[test]
    fn parse_latency_bare_number() {
        assert_eq!(parse_latency("3.5"), Some(3.5));
    }

    #[test]
    fn extract_json_object_result() {
        let mut params = HashMap::new();
        let mut result_map = serde_json::Map::new();
        result_map.insert("user_id".into(), serde_json::Value::from(42));
        result_map.insert("name".into(), serde_json::Value::from("alice"));
        params.insert("result-body".into(), serde_json::Value::Object(result_map));

        let mp = extract_model_params(&params);
        let Some(ResultDef::Json(v)) = &mp.result else {
            panic!("expected Json result")
        };
        assert_eq!(v["user_id"], serde_json::Value::from(42));
        assert_eq!(v["name"], serde_json::Value::from("alice"));
    }

    #[test]
    fn extract_json_array_result_for_indexed_captures() {
        // The shape a phase-poll `until:` predicate reads via
        // `/0/value`, `/1/value:count`, … — author-controlled state
        // that satisfies the predicate with no live backend.
        let mut params = HashMap::new();
        params.insert(
            "result-body".into(),
            serde_json::json!([{ "value": 1 }, { "value": [] }, { "value": 0 }]),
        );
        let mp = extract_model_params(&params);
        let Some(ResultDef::Json(v)) = &mp.result else {
            panic!("expected Json result")
        };
        assert_eq!(v.pointer("/0/value"), Some(&serde_json::Value::from(1)));
        assert_eq!(v.pointer("/2/value"), Some(&serde_json::Value::from(0)));
    }

    #[test]
    fn explicit_null_result_is_no_body() {
        let mut params = HashMap::new();
        params.insert("result-body".into(), serde_json::Value::Null);
        assert!(extract_model_params(&params).result.is_none());
    }

    #[test]
    fn extract_error_params() {
        let mut params = HashMap::new();
        params.insert("result-error-rate".into(), serde_json::Value::from(0.05));
        params.insert(
            "result-error-name".into(),
            serde_json::Value::from("Timeout"),
        );

        let mp = extract_model_params(&params);
        assert_eq!(mp.error_rate, 0.05);
        assert_eq!(mp.error_name, "Timeout");
    }

    #[test]
    fn extract_saturation_params() {
        let mut params = HashMap::new();
        params.insert("result-capacity".into(), serde_json::Value::from(4));
        params.insert("result-overload".into(), serde_json::Value::from("16"));

        let mp = extract_model_params(&params);
        assert_eq!(mp.capacity, Some(4));
        assert_eq!(mp.overload, Some(16));
    }

    #[test]
    fn saturation_zero_disables() {
        // A literal zero means "no limit," so the user doesn't have
        // to remove the field to unset it.
        let mut params = HashMap::new();
        params.insert("result-capacity".into(), serde_json::Value::from(0));
        params.insert("result-overload".into(), serde_json::Value::from(0));

        let mp = extract_model_params(&params);
        assert_eq!(mp.capacity, None);
        assert_eq!(mp.overload, None);
    }

    #[tokio::test]
    async fn overload_rejects_when_in_flight_exceeds_threshold() {
        // Capacity 1, overload 2: exactly 2 ops fit (1 serving + 1
        // queued). A third concurrent op must reject with Overload.
        let adapter = ModelAdapter::new();
        let mut op = nmbrs_workload::model::ParsedOp::simple("test", "SELECT 1;");
        op.params
            .insert("result-latency".into(), serde_json::Value::from("50ms"));
        op.params
            .insert("result-capacity".into(), serde_json::Value::from(1));
        op.params
            .insert("result-overload".into(), serde_json::Value::from(2));

        let dispenser: Arc<dyn OpDispenser> =
            Arc::from(adapter.map_op(&op, test_kernel()).await.unwrap());
        let fields = Arc::new(ResolvedFields::new(
            vec!["stmt".into()],
            vec![polydat::ast::Value::Str("SELECT 1;".into())],
        ));

        let mut handles = Vec::new();
        for cycle in 0..3u64 {
            let d = dispenser.clone();
            let f = fields.clone();
            handles.push(tokio::spawn(async move {
                let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
                let ctx = nmbrs_runtime::adapter::ExecCtx::new(&f, &pulls);
                d.execute(cycle, &ctx).await
            }));
        }
        let mut overload_count = 0usize;
        for h in handles {
            let res = h.await.expect("task panicked");
            if let Err(ExecutionError::Op(e)) = &res
                && e.error_name == "Overload"
            {
                overload_count += 1;
            }
        }
        assert_eq!(
            overload_count, 1,
            "expected exactly one op to be rejected with Overload, got {overload_count}"
        );
    }

    #[tokio::test]
    async fn model_dispenser_basic() {
        let adapter = ModelAdapter::new();
        let template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT 1;");
        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();
        let fields = ResolvedFields::new(
            vec!["stmt".into()],
            vec![polydat::ast::Value::Str("SELECT 1;".into())],
        );
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let ctx = nmbrs_runtime::adapter::ExecCtx::new(&fields, &pulls);
        let result = dispenser.execute(0, &ctx).await.unwrap();
        assert!(result.body.is_some());
    }

    #[test]
    fn extract_model_params_picks_up_throw_at_numeric() {
        let mut params = HashMap::new();
        params.insert("result-throw-at".into(), serde_json::Value::from(42u64));
        let mp = extract_model_params(&params);
        assert_eq!(mp.throw_at_cycle, Some(42));
        // Default name when only throw-at is set.
        assert_eq!(mp.throw_name, "ThrowAt");
    }

    #[test]
    fn extract_model_params_picks_up_throw_at_string() {
        // Workload YAML often interpolates a binding into result-throw-at,
        // which arrives at the adapter as a string after expansion.
        let mut params = HashMap::new();
        params.insert("result-throw-at".into(), serde_json::Value::from("17"));
        params.insert(
            "result-throw-name".into(),
            serde_json::Value::from("staircase"),
        );
        let mp = extract_model_params(&params);
        assert_eq!(mp.throw_at_cycle, Some(17));
        assert_eq!(mp.throw_name, "staircase");
    }

    #[tokio::test]
    async fn driver_level_throw_at_fires_on_threshold_cycle() {
        let mut params = HashMap::new();
        params.insert("result-throw-at".into(), serde_json::Value::from(3u64));
        params.insert(
            "result-throw-name".into(),
            serde_json::Value::from("staircase"),
        );

        let adapter = ModelAdapter::new();
        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT 1;");
        template.params = params;
        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();

        let fields = ResolvedFields::new(
            vec!["stmt".into()],
            vec![polydat::ast::Value::Str("SELECT 1;".into())],
        );
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let ctx = nmbrs_runtime::adapter::ExecCtx::new(&fields, &pulls);

        // Cycles below threshold succeed.
        for c in [0u64, 1, 2] {
            let r = dispenser.execute(c, &ctx).await;
            assert!(r.is_ok(), "cycle {c} below threshold should succeed");
        }
        // Cycle == threshold trips.
        let r = dispenser.execute(3, &ctx).await;
        match r {
            Err(ExecutionError::Op(e)) => {
                assert_eq!(e.error_name, "staircase");
                assert!(
                    e.message.contains("3"),
                    "message should name the cycle: {}",
                    e.message
                );
            }
            other => panic!("expected throw-at op error, got {other:?}"),
        }
        // Cycles past threshold succeed (the op is not stuck).
        for c in [4u64, 5] {
            let r = dispenser.execute(c, &ctx).await;
            assert!(r.is_ok(), "cycle {c} past threshold should succeed");
        }
    }
}

// =========================================================================
// Adapter Registration (inventory-based, link-time)
// =========================================================================

inventory::submit! {
    nmbrs_runtime::adapter::AdapterRegistration {
        names: || &["testkit"],
        known_params: || &[
            "result-body", "result-load", "result-latency",
            "result-error-rate", "result-error-name", "result-error-message",
            "result-capacity", "result-overload",
            "result-throw-at", "result-throw-name",
        ],
        display_preference: |_params| nmbrs_runtime::adapter::DisplayPreference::Auto,
        supported_controls: || &[],
        create: |params| Box::pin(async move {
            Ok(std::sync::Arc::new(ModelAdapter::with_config(ModelConfig {
                stdout: StdoutConfig::from_params(&params),
                diagnose: false,
            })) as std::sync::Arc<dyn nmbrs_runtime::adapter::DriverAdapter>)
        }),
    }
}

// SRD-35 Push C: testkit adapter declares itself
// pool-shareable. `ModelAdapter` is pure in-process
// state with no external resources — sharing it across
// phases is trivially correct (and avoids re-parsing
// the result-* config knobs every phase). The whole
// `result-*` family is identity-bearing because the
// adapter's per-call behaviour is configured at
// construction.
inventory::submit! {
    nmbrs_runtime::adapter::SharedDriverRegistration {
        adapter: "testkit",
        driver: nmbrs_runtime::adapter::DEFAULT_DRIVER_NAME,
        share_capability: nmbrs_runtime::resource_pool::ShareCapability::Shared,
        resource_key: |params| {
            let identity_fields = [
                "result-body", "result-load", "result-latency",
                "result-error-rate", "result-error-name", "result-error-message",
                "result-capacity", "result-overload",
                "result-throw-at", "result-throw-name",
            ];
            let mut k = nmbrs_runtime::resource_pool::ResourceKey::new("testkit");
            for field in identity_fields {
                if let Some(v) = params.get(field) {
                    k = k.with(field, v.clone());
                }
            }
            Ok(k)
        },
    }
}
