# SRD-109 — Web-Only Client Drivers on the HTTP Module

Status: IMPLEMENTED.
Builds directly on SRD-108: a web-only client driver is an
SRD-108 implementation module whose op bodies are http op
templates, plus a thin registration layer that gives it
classic-driver ergonomics.

## Problem

Many targets — REST-native vector databases foremost — ship
"client drivers" that are, on the wire, nothing but HTTP
requests. Reaching them today means either writing a native Rust
adapter per vendor (the per-vendor cost the VDBBench gap analysis
identified as the blocking adapter-reach gap) or hand-writing raw
http workloads per benchmark. Neither scales, and a native
adapter for a plain-HTTP protocol also *hides* the literal
request from the operator.

The op-authenticity principle decides the shape: for a web-only
client, the authentic native usage IS the HTTP request. A driver
for such a client should therefore surface literal requests —
never invent harness-side structured protocol fields (the C8
ruling) — and cost zero Rust per vendor.

## Part 1 — the driver library (an SRD-108 implementation module)

A vendor driver is a bundled implementation workload:

```yaml
# nmbrs/drivers/vendorx/vector_impl.yaml
implements: vector_suite_blueprint

params:
  base_url: "http://localhost:6333"
  api_key: ""

phases:
  load_train:
    ops:
      insert:
        method: PUT
        uri: "{base_url}/collections/{table}/points"
        headers:
          api-key: "{api_key}"
        body: |
          {"points":[{"id":"{id}","vector":{train_vector}}]}
  serial_probe:
    ops:
      select_ann:
        method: POST
        uri: "{base_url}/collections/{table}/points/search"
        body: |
          {"vector":{query_vector},"limit":{suite_limit},"with_payload":true}
        result:
          keys: result[*].id   # Part 3 — fills the results: interface
```

Everything SRD-108 established applies unchanged: the blueprint
owns scaffolding and relevancy; binding is load-time;
interface types prove at pre-map synthesis; SRD-107 provenance
covers the request shapes, so switching vendors re-runs
idempotent prereqs correctly.

## Part 2 — the driver manifest (registration + defaults)

A small manifest gives library drivers the ergonomics of built-in
adapters without any new op vocabulary:

```yaml
# nmbrs/drivers/vendorx/driver.yaml
driver: vendorx              # must match the directory name
adapter: http
library: vector_impl         # sibling implementation workload
description: VendorX REST vector client (native HTTP op forms)
defaults:
  params:
    base_url: "http://localhost:8099"
```

- `driver=vendorx` on the CLI resolves the manifest (local
  `./drivers/vendorx/driver.yaml` first, then the bundled catalog
  entry `drivers/vendorx/driver`; both at once is a hard error):
  the library lands as `workload=` when none was given (its
  `implements:` pulls the blueprint) or as `impl=` when a
  blueprint was invoked; the backing adapter and the manifest's
  default params fill only absent keys, so the CLI always wins
  and the defaults still overlay the library's own declared
  params. A `driver=` value matching no manifest keeps its legacy
  meaning (an alias for `adapter=`). `impl=` alongside `driver=`
  is a conflict error.
- `nmbrs describe drivers` lists manifests (bundled + local),
  mirroring `describe workloads`.
- The manifest maps NAMES to templates and defaults — it never
  defines op fields. The op surface remains exactly the http
  module's (`method`/`uri`/`body`/`headers`), so every request
  stays literal and operator-visible. Error policy is likewise
  the library's own (phase-level `errors:` in the workload), not
  manifest matter.

## Part 3 — the `results:` interface (typed result shapes)

SRD-108 left probe yields empty because relevancy traverses the
result body. For HTTP drivers the result body is vendor JSON, so
the implementation MUST normalize — and the contract deserves the
same treatment as the other two interface legs. `abstract:` gains
a third section:

```yaml
# blueprint side — names + types only; paths are protocol matter
ops:
  select_ann:
    abstract:
      needs:
        query_vector: vec_f32
      results:
        keys: vec_i64        # projected neighbor-key column
    evaluations:
      relevancy:
        actual: keys          # bare wire name — typed read
        expected: ground_truth
```

Each `results:` name is a **wire** the bound op delivers every
cycle by projecting the response body; the type is the projected
wire's polydat type (`vec_i64`, `vec_f64`, `json`, …). The three
legs are now symmetric, and each is delivered through an
*ordinary* surface — no new implementation-side vocabulary:

| leg | blueprint declares | implementation delivers via |
|---|---|---|
| `needs` | name → type | ordinary `{name}` references |
| `yields` | name → type | ordinary `captures:` |
| `results` | name → type | ordinary `result:` bindings |

```yaml
# implementation side — the ordinary SRD-66 result-binding surface
select_ann:
  method: POST
  uri: "{base_url}/collections/{table}/search"
  body: '{"vector": {query_vector}, "limit": {suite_limit}}'
  result:
    keys: result[*].id      # SRD-70 wildcard column projection
```

- **Wildcard projection** (`[*]`) is the SRD-70 first-wave shape,
  implemented here: one wildcard per path, projecting a column
  across every element of the matched array into a typed vector
  (uniform coercion: all-int → `VecI64`, all-float → `VecF64`,
  otherwise the collected array as `Json`). `[*].key` projects a
  top-level body array (the CQL shape); `result[*].id` projects a
  nested one (typical vendor JSON).
- **Checks, per the SRD-108 table**: at load, every `results:`
  name must appear as a result-binding LHS on the bound op
  (unbound name = error naming the slot and the remedy), and a
  blueprint-side `result:` entry for an interface name is a
  collision (paths are protocol matter). At pre-map synthesis the
  wire's kernel slot is *declared from* the interface type
  (`PortType::from_keyword`) and verified by the same
  `verify_op_interface` pass as needs/yields — the projection
  lands on a correctly-typed slot or the workload does not build.
- **Evaluations read wires**: `relevancy.actual:` resolves
  wire-first (`ctx.wires.get`, typed extraction), falling back to
  the legacy result-column walk for workloads that predate the
  interface. The blueprint pair migrates to the wire form.

## Resolved-by-design questions

- **Auth flows** (login → token → refresh): expressed as ordinary
  ops — the blueprint declares a `connect` phase with an abstract
  `establish` slot; a web driver binds login there and captures
  the token into a shared wire its later ops reference in headers
  (the token wire is protocol-private: declared by the
  implementation's workload-level bindings, never by the
  blueprint). Token EXPIRY mid-run is v2 (a poll/refresh daemon
  op is expressible today; codify the pattern when a real vendor
  needs it).

## Non-goals

- gRPC / custom-binary vendors: real adapters, out of scope here.
- Multi-vendor comparison orchestration (running N drivers in one
  invocation): composes at the session/report layer, not here.
- Any harness-side structured protocol vocabulary (C8 stands).

## Deliverables

1. Driver manifest model + resolution (`driver=` param, manifest
   discovery local-first then bundled, defaults overlay) +
   `describe drivers`.
2. The `results:` interface (Part 3) delivered through ordinary
   result-bindings, with load + synthesis checks per SRD-108's
   table, and SRD-70 wildcard projection as the runtime leg.
3. First vendor library (`drivers/vendorx`) implementing
   `vector_suite_blueprint`, with the auth-flow `connect` phase
   as the named test case.
4. e2e: a blueprint bound to an http implementation against a
   mock endpoint, plus walker examples (testkit `result-body`
   for standalone projection round-trips).
