# SRD 73: Op Field Modifiers — Monadic Compose for Per-Op Adapter Settings

*(DRAFT — not yet implemented)*

## Motivation

Adapters routinely need a way for workload authors to override
specific driver knobs on a per-op basis without forcing every op to
carry every knob. A CQL op that wants a 5-minute timeout for a slow
DROP INDEX shouldn't have to also declare consistency, page size,
serial consistency, etc., and the absence of a knob on the op
template must mean "the driver's native default applies" — not
"reset the driver to nmbrs's default at this point".

The current CQL adapter applies `request_timeout_ms` at **session
construction time** (`cluster.set_request_timeout`,
`builder.connection_timeout`). It's a workload-param surface and
flows through the resource-pool shell, but in practice the value is
baked into the session once at connect and not re-applied per
statement. There is no per-op override, and the documented
"per-phase shell" contract from SRD 35 isn't actually honored
end-to-end.

This SRD introduces:

1. A generic **`OpFieldModifier<T>`** trait + **`ModifierChain<T>`**
   composer in `nmbrs-runtime` (the adapter API crate). One
   abstraction every adapter can reuse.
2. A **GK-scoped initializer-time resolution** pattern that mirrors
   the upstream nosqlbench `enhanceFuncOptionally` (a.k.a.
   "enhance function" / monadic compose) approach: the dispenser's
   initializer queries the live Polydat scope chain once per name in
   its universal-field selector list, captures the resolved value
   into a modifier struct, and stores the resulting chain on the
   dispenser. Per-cycle execution is just `chain.apply(&mut stmt)`
   — zero re-evaluation, zero scope walk.
3. An optional **`ModifierTraceSink`** for cross-cutting observation
   (event-log JSON or `tracing` crate emission) with lazy
   gating — JSON serialization is paid only when an active sink
   actually wants the value.
4. The **CQL universal field superset** (consistency,
   serial_consistency, request_timeout_ms, page_size, cql_trace)
   plumbed through the trait, with per-engine modifier impls
   bridging into each driver's per-statement API.

## Background: the upstream pattern

The upstream NB Java adapter for CQL ([Cqld4BaseOpDispenser]) builds
an "enhanced statement function" by chaining a series of optional
modifications, where each modification is a NO-OP unless the user
explicitly specified its field on the op template:

```java
LongFunction<S> partial = basefunc;
partial = op.enhanceFuncOptionally(partial, "cl", DefaultConsistencyLevel.class,
    (s, cl) -> s.setConsistencyLevel(cl));
partial = op.enhanceFuncOptionally(partial, "timeout", double.class,
    (s, l) -> s.setTimeout(Duration.ofMillis((long)(l*1000))));
// ... one call per knob
return partial;  // critical path = partial.apply(cycle)
```

`enhanceFuncOptionally` is mechanically a monadic bind:
`func ↦ if user_set(field) then func ∘ modifier else func`.
It produces a closure that has CURRIED IN only the fields the user
specified; default-handling stays the driver's concern.

Two phases, not one:

- **Initializer phase** (once per `OpDispenser` construction): the
  enhancer-chain is composed by querying the op template for each
  field; absent fields contribute nothing to the chain. The
  resulting `LongFunction<S>` is the dispenser's specialized
  statement-builder.
- **Critical-path phase** (per cycle): `partial.apply(cycle)`. No
  re-query, no name lookup, no map access. Just the captured
  closures with their captured values.

This is a Rust-flavoured port of the same shape: `Box<dyn
OpFieldModifier<T>>` plays the role of the per-knob closure;
`ModifierChain<T>` plays the role of the composed `LongFunction<S>`.

[Cqld4BaseOpDispenser]: ../../links/nosqlbench/nb-adapters/adapter-cqld4/src/main/java/io/nosqlbench/adapter/cqld4/opdispensers/Cqld4BaseOpDispenser.java

## Surface

### `OpFieldModifier<T>` trait

Lives in `nmbrs-runtime::adapter` (the adapter API surface). Generic
over the target type so each adapter can specialize for its driver's
per-statement type:

```rust
pub trait OpFieldModifier<T>: Send + Sync + 'static {
    /// User-facing field name. Matches the op-template key and the
    /// adapter's universal-field selector list. Returned as a
    /// `&'static str` so trace sinks can keep cheap references.
    fn field_name(&self) -> &'static str;

    /// Mutate the target. Constructed only when the user set the
    /// field; the chain pre-filters at build time, so `apply` is
    /// branchless on activeness.
    fn apply(&self, target: &mut T);

    /// Structured diagnostic value for the event-log emission.
    /// Called LAZILY — only when a trace sink is present AND the
    /// sink decides the closure should run. See `ModifierChain::apply`.
    fn diagnostic_value(&self) -> serde_json::Value;
}
```

There is no `is_active()` method. The chain itself filters inactive
modifiers at build time; anything in `chain.active` is by
construction active.

### `ModifierChain<T>` composer

The composed enhancer — the moral equivalent of upstream's
`LongFunction<S>`:

```rust
pub struct ModifierChain<T> {
    op_label:   String,
    active:     Vec<Box<dyn OpFieldModifier<T>>>,
    event_sink: Option<Arc<dyn ModifierTraceSink>>,
}

impl<T> ModifierChain<T> {
    /// Built once in the dispenser initializer. Inactive modifiers
    /// (those where the user did not bind the corresponding name in
    /// the Polydat scope) are NOT included — caller dropped them before
    /// pushing.
    pub fn new(
        op_label: impl Into<String>,
        active:   Vec<Box<dyn OpFieldModifier<T>>>,
        event_sink: Option<Arc<dyn ModifierTraceSink>>,
    ) -> Self { ... }

    pub fn is_empty(&self) -> bool { self.active.is_empty() }

    /// Critical-path entry. Called per cycle, immediately before the
    /// adapter binds values into the statement / sends it.
    pub fn apply(&self, target: &mut T) {
        match &self.event_sink {
            None => {
                for m in &self.active {
                    m.apply(target);  // hot path — no closures, no JSON
                }
            }
            Some(sink) => {
                for m in &self.active {
                    m.apply(target);
                    sink.modifier_applied(
                        &self.op_label,
                        m.field_name(),
                        &|| m.diagnostic_value(),
                    );
                }
            }
        }
    }
}
```

The `None` arm is the no-observer hot path: pure direct dispatch
through the modifier's own `apply`. Zero allocation, zero JSON
serialization, zero closure construction.

### `ModifierTraceSink` cross-cutting hook

```rust
pub trait ModifierTraceSink: Send + Sync {
    /// Called from `ModifierChain::apply` when a modifier fires AND
    /// a sink is installed. The `value_fn` closure is invoked only
    /// if the sink itself decides to record this event — sinks
    /// SHOULD check their own filter (`tracing::event_enabled!`,
    /// event-log subscription state, etc.) before calling the
    /// closure, so JSON serialization of `diagnostic_value` is paid
    /// only when a consumer will read it.
    fn modifier_applied(
        &self,
        op:       &str,
        field:    &'static str,
        value_fn: &dyn Fn() -> serde_json::Value,
    );
}
```

Built-in sinks (both in `nmbrs-runtime`):

- **`TracingTraceSink`** — emits a `tracing::trace!(target:
  "nmbrs::op_modifier", op, field, value = ?value_fn())` event,
  gated on `tracing::enabled!(target: "nmbrs::op_modifier", Level::TRACE)`.
  Default sink for development.
- **`JsonEventSink`** — writes `{ op, field, value }` JSON records
  to the SRD-44a checkpoint event log when a per-session config
  selects it. The session config picks the sink installation; the
  modifier abstraction itself is agnostic.

Sessions install at most one sink at construction. (Composition
sinks — fan-out to multiple — are a trivial wrapper; deferred unless
demanded.)

### Polydat as the single name-resolution path

The dispenser initializer resolves universal-field names through the
GK scope chain. **No reach-around** — the `params:` block, scenario-
tree `set:` shadows, `bindings:`, and per-op fields all surface
through the existing `PolydatKernel::lookup` chokepoint (see SRD 13c,
13f, 67). This SRD adds no new resolution layer.

The dispenser's initializer:

```rust
// In crates/nmbrs-adapter-cql/src/<engine>/<dispenser>.rs, inside map_op:
fn build_cql_modifier_chain<S>(
    parent: &PolydatKernel,
    op_label: &str,
    sink:     Option<Arc<dyn ModifierTraceSink>>,
) -> Result<ModifierChain<S>, String>
where
    <Engine>: CqlModifierBuilder<Statement = S>,
{
    let mut active: Vec<Box<dyn OpFieldModifier<S>>> = Vec::new();

    for field in CQL_UNIVERSAL_FIELDS {
        let Some(value) = parent.lookup(field.name) else {
            continue;   // user did not set; native driver default in force
        };
        active.push(<Engine>::modifier_for(field, value)?);
    }

    Ok(ModifierChain::new(op_label, active, sink))
}
```

`PolydatKernel::lookup` (already shipped, polydatkernel.rs:491) walks folded
outputs → extern auto-passthrough → shared cells, returning
`Option<Value>`. It is THE name-resolution chokepoint; this SRD
re-uses it as-is.

The selector list mechanism — `CQL_UNIVERSAL_FIELDS` is the
adapter's declaration of "these are the names I care about" — is
the only new piece. Each adapter declares its own list; the
resolution machinery is shared.

### CQL universal field superset

Universal across both the `scylla` and `cassandra-cpp` engines. The
chosen names are CQL-spec spellings where the drivers diverge, with
explicit unit suffixes where SI ambiguity would matter.

| Universal name        | scylla method                          | cassandra-cpp method                    | Type           |
|-----------------------|----------------------------------------|------------------------------------------|----------------|
| `consistency`         | `set_consistency(Consistency)`         | `set_consistency(Consistency)`           | string (enum)  |
| `serial_consistency`  | `set_serial_consistency(...)`          | `set_serial_consistency(...)`            | string (enum)  |
| `request_timeout_ms`  | `set_request_timeout(Option<Duration>)`| `set_statement_request_timeout(Option<Duration>)` | u64 (millis) |
| `page_size`           | `set_page_size(i32)`                   | `set_paging_size(i32)`                   | i64            |
| `cql_trace`           | `set_tracing(bool)`                    | `set_tracing(bool)`                      | bool           |

Naming choices, explicitly documented:

- `request_timeout_ms` — explicit unit suffix beats upstream Java's
  `timeout` (double seconds). Matches the existing nmbrs workload-
  param name; the session-level setting and the per-op setting use
  ONE name.
- `page_size` — CQL spec spelling; beats cassandra-cpp's
  `paging_size`.
- `consistency` / `serial_consistency` — full words; drop upstream
  Java's `cl` / `scl` shorthands.
- `cql_trace` — see §"Tracing terminology" below.

### Deferred (engine asymmetry — not in initial superset)

- `idempotent` — vendored cassandra-cpp Rust binding does not yet
  expose `cass_statement_set_is_idempotent`. Add to vendored crate
  in a separate change before adding to the superset.
- `custom_payload`, `retry_policy`, `execution_profile_name` —
  engine-specific or asymmetric surfaces. Future SRD if/when needed.

### Tracing terminology — three orthogonal concepts

The word "tracing" is dangerously overloaded. This SRD pins down
three separate concepts that must not be conflated:

| Concept                                  | What it is                                                                             | nmbrs surface                                                          |
|------------------------------------------|----------------------------------------------------------------------------------------|------------------------------------------------------------------------|
| **CQL query-tracing subsystem**          | Cassandra server-side query trace; rows written to `system_traces.*` on the cluster. A data SOURCE — independent of how nmbrs records or logs the result. | Per-op: `cql_trace: true` (this SRD). Workload-level stochastic: `trace_rate` (existing CQL config). |
| **Rust `tracing` crate log severity**    | The `tracing` crate's `trace!`/`debug!`/`info!`/… emission filter; a LOG-LEVEL knob. Orthogonal to data sources. | `RUST_LOG=...=trace` env var. Not a workload field.                    |
| **nmbrs event-log emission**             | Structured records written to the SRD-44a checkpoint JSONL or other registered event sinks. A separate channel from log levels. | `ModifierTraceSink::JsonEventSink` (this SRD) and other event subscribers. |

`cql_trace` engages concept #1 for a specific statement. Whether
the resulting `system_traces` rows surface in concept #2 or #3 is
independent of `cql_trace` itself — those are downstream collector
concerns.

The existing workload-level `trace_rate` (stochastic sampling
probability per cycle) coexists with per-op `cql_trace` (forced for
this statement): same subsystem, different policy.

## Worked example: per-op timeout on the idx_sweep cleanup

A cleanup teardown phase that wants a 5-minute per-statement
timeout, with the rest of the workload's CQL ops keeping the
session default:

```yaml
phases:
  teardown_cleanup:
    concurrency: 1
    ops:
      drop_vector_index:
        raw: "DROP INDEX IF EXISTS {keyspace}.{table}{vector_idx_suffix}"
        request_timeout_ms: 300000   # universal per-op field
      drop_metadata_index:
        raw: "DROP INDEX IF EXISTS {keyspace}.{table}{meta_idx_suffix}"
        request_timeout_ms: 300000
      drop_table:
        raw: "DROP TABLE IF EXISTS {keyspace}.{table}"
        request_timeout_ms: 300000
```

What happens:

1. Workload-parser routes the per-op `request_timeout_ms` field into
   the op-template's Polydat matter (via the standard SRD-13d op-template
   scope synthesis).
2. At adapter `map_op` time, the dispenser initializer calls
   `parent.lookup("request_timeout_ms")` for each universal field.
   For these ops it returns `Some(Value::U64(300_000))`; for ops
   that don't set it, it returns `None`.
3. The dispenser pushes a `RequestTimeoutMod { timeout: 5min }`
   onto the chain. Other modifiers — `consistency`, `page_size`,
   etc. — are not added because the Polydat scope doesn't bind them.
4. Per cycle, `chain.apply(&mut stmt)` calls
   `stmt.set_request_timeout(Some(5min))` and nothing else.

A workload-level setting still works via the same mechanism:

```yaml
params:
  request_timeout_ms: 60000   # 60s default for every CQL op in this workload

phases:
  teardown_cleanup:
    ops:
      drop_vector_index:
        raw: "..."
        request_timeout_ms: 300000   # this op overrides to 5min
      drop_table:
        raw: "..."                   # this op uses the 60s workload default
```

The workload-param sets up a folded-output binding visible to all
phases via the Polydat scope chain. The per-op field shadows it inside
that op's scope. Both reach the dispenser via `lookup`.

## Internal model

### Per-engine modifier impls

Each CQL engine module owns a small file `op_modifier.rs` with one
modifier impl per universal field:

```rust
// crates/nmbrs-adapter-cql/src/scylla/op_modifier.rs
struct RequestTimeoutMod { timeout: Duration }

impl OpFieldModifier<ScyllaStatement> for RequestTimeoutMod {
    fn field_name(&self) -> &'static str { "request_timeout_ms" }
    fn apply(&self, stmt: &mut ScyllaStatement) {
        stmt.set_request_timeout(Some(self.timeout));
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::from(self.timeout.as_millis() as u64)
    }
}

// Factory function the dispenser-initializer calls:
pub fn modifier_for(field: &CqlFieldDecl, value: Value)
    -> Result<Box<dyn OpFieldModifier<ScyllaStatement>>, String>
{
    match field.name {
        "request_timeout_ms" => {
            let ms = value.as_u64().ok_or("request_timeout_ms: expected u64")?;
            Ok(Box::new(RequestTimeoutMod { timeout: Duration::from_millis(ms) }))
        }
        "consistency" => { ... }
        ...
    }
}
```

The cassandra-cpp file is structurally identical with engine-
specific setter calls.

### Op-template field plumbing

For the per-op fields to reach the Polydat scope, the workload parser
must route them as op-template matter, not adapter sidecar. SRD 13d
already specifies the op-template scope synthesis path — universal
fields fall under that umbrella naturally. The workload-parser
change is to add the universal field names to the allow-list for
op-template fields (so they're not rejected as unknown).

Each adapter's `known_op_fields()` declaration is extended with its
own universal field names. The CQL adapter declares the five from
the table above.

### Precedence — single chain through GK

| Source                                              | Resolves at                  | Wins when both set |
|-----------------------------------------------------|------------------------------|---------------------|
| Driver native default                               | session connect              | (loser)             |
| Workload `params: { request_timeout_ms: 60000 }`   | session connect             | overrides default   |
| Per-op field on op template                         | per-statement                | wins over both      |

This is NOT a new precedence machine. It's the existing Polydat scope-
chain `lookup` resolution order (folded outputs → extern auto-
passthrough → shared cells; SRD 13c §"Visibility Rules"). Per-op
fields land deepest in the scope chain because the op-template is
the innermost scope; they shadow workload-level bindings via the
standard SRD-16 walk-up.

### Cost model

- **Hot path with no sink** — one virtual call per active modifier;
  one direct setter call inside. For typical ops (0-2 active
  modifiers), this is two indirect calls per execute.
- **Hot path with sink, sink disabled by filter** — additional one
  call to `sink.modifier_applied(...)` per active modifier; sink's
  internal filter check elides the closure invocation. Cost ≈ one
  Atomic load per call.
- **Hot path with sink, sink enabled** — adds the closure dispatch
  + `diagnostic_value` JSON serialization. Paid only when consumed.
- **No active modifiers** — `ModifierChain::is_empty()` returns
  true; the dispenser can skip the `apply` call entirely.

## Phased delivery

**P1 — Trait + chain + sinks** (`nmbrs-runtime`). Generic
`OpFieldModifier<T>`, `ModifierChain<T>`, `ModifierTraceSink` +
`TracingTraceSink` impl. Unit tests on a synthetic target type.
No adapter changes.

**P2 — CQL universal field surface + per-engine wiring**.
`CQL_UNIVERSAL_FIELDS` selector list in `crates/nmbrs-adapter-cql/src/common/
op_modifier.rs`. Per-engine modifier impls. Dispenser-initializer
wiring (`map_op` builds the chain via `parent.lookup`). Op-template
field plumbing so per-op fields reach Polydat scope. `known_op_fields()`
extension.

**P3 — Workload migration**. `idx_sweep` cleanup ops switch to per-
op `request_timeout_ms: 300000`. SRD-44a JSON event-log sink
plumbing (when the session config selects it). Any additional
adapters that want the same pattern subscribe their own universal-
field list.

## Open questions

- **Per-op vs op-template-level bindings:** the worked example
  writes `request_timeout_ms: 300000` as an op-field. Should the
  same name also be accepted inside an op-template `bindings:`
  block (allowing cycle-dependent timeouts via Polydat expressions)?
  Proposed: yes — `bindings:` already lands in the op-template GK
  scope, so `lookup` finds it the same way. The modifier is built
  from whatever value the scope yields at initializer time; if the
  Polydat binding is effectively-const (the usual case) the dispenser
  captures the constant. Per-cycle-dynamic timeouts are deferred
  to a future SRD if and when a real use case appears.
- **Sink composition (fan-out):** for now, sessions install at
  most one `ModifierTraceSink`. If both tracing-crate emission and
  JSON event-log emission are desired simultaneously, a trivial
  `CompositeSink { children: Vec<Arc<dyn ModifierTraceSink>> }`
  covers it; ship when demanded.
- **HTTP adapter migration:** HTTP currently uses per-op
  `request_timeout_ms` directly on op fields (see `jolokia_*` in
  the CQL vector workload). It's an inconsistent surface vs CQL
  today. Once this SRD ships for CQL, HTTP should migrate to the
  same trait — its modifier targets would be `reqwest::Request`
  or the engine equivalent. Tracked as a follow-up, not in scope
  here.

## Non-goals

- **Per-cycle dynamic per-op fields.** All universal-field values
  are captured once at initializer time. Cycle-varying values
  would require a different shape (closures from cycle → Value).
  Not blocked by this SRD, but not delivered either.
- **Cross-adapter universal fields.** Each adapter declares its
  own selector list. There is no global "all adapters know about
  `request_timeout_ms`" registry — adapters opt in explicitly,
  with engine-appropriate semantics.
- **Modifying the cluster/session-level surface.** Workload-level
  `params: { request_timeout_ms: X }` continues to flow through
  the session-construction path AND through `lookup` for per-op
  resolution. This SRD doesn't remove the session-level setting;
  it just lets per-op shadowing actually work.

## See also

- [SRD 30](30_adapter_interface.md) — `DriverAdapter` / `OpDispenser`
  contract; `map_op` is the initializer entry point.
- [SRD 13c](13c_polydat_scope_model.md) — `bind_outer_scope`, manifest
  extraction, visibility rules. `lookup` is documented here.
- [SRD 13d](13d_op_template_scope.md) — op-template Polydat scope
  synthesis; the path by which per-op fields enter the scope chain.
- [SRD 13f](13f_cross_scope_wire_materialization.md) — read
  invariant + matter-AST classification; per-op fields are an
  instance of the inlined-constant gradient.
- [SRD 35](35_driver_resources.md) — adapter-shell vs instance-
  shaping param split; this SRD's selector list is per-instance
  shell-shaping.
- [SRD 44a](44a_checkpoint_jsonl.md) — JSON event-log emission;
  the `JsonEventSink` target.
- [SRD 50](50_cql_adapter.md) — CQL adapter specifics; per-engine
  modifier impls land alongside the existing per-engine modules.
- [SRD 74](74_none_propagation.md) — None propagation through
  string interpolation. Required for the `set:` desugar in this
  SRD to behave correctly when an iter-var-driven shadow can't
  bind: Rule 1 (shipped) makes the bad path produce
  `Value::None` instead of `Str("None")`; conditional-shadow
  `const` (designed) makes lookup fall through to upstream
  defaults transparently.
