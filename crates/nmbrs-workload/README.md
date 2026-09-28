# nmbrs-workload

The nmbrs workload model and its YAML parser. The crate turns a workload
file, or the inline `op='...'` shorthand, into a normalized `Workload`
containing phases, scenarios, ops, bindings, params and a report definition.
It parses only and executes nothing. Execution belongs to
[`nmbrs-runtime`](https://crates.io/crates/nmbrs-runtime).

## Where it sits in nmbrs

- Depends on [`polydat`](https://crates.io/crates/polydat), which owns the
  comprehension AST that `for_each` clauses parse into.
- Used by:
  - [`nmbrs-runtime`](https://crates.io/crates/nmbrs-runtime)
  - the [`nmbrs`](https://crates.io/crates/nmbrs) CLI
  - every adapter crate, because `DriverAdapter::map_op` receives a
    `nmbrs_workload::model::ParsedOp`
    ([`nmbrs-adapter-stdout`](https://crates.io/crates/nmbrs-adapter-stdout),
    [`nmbrs-adapter-http`](https://crates.io/crates/nmbrs-adapter-http),
    [`nmbrs-adapter-cql`](https://crates.io/crates/nmbrs-adapter-cql),
    [`nmbrs-adapter-testkit`](https://crates.io/crates/nmbrs-adapter-testkit),
    [`nmbrs-adapter-plotter`](https://crates.io/crates/nmbrs-adapter-plotter),
    [`nmbrs-adapter-openapi`](https://crates.io/crates/nmbrs-adapter-openapi)).

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs) CLI
and write workload YAML. Depend on this crate when you need to read, inspect
or check workloads from Rust. Adapter authors need it for the `ParsedOp` type.

## The workload model (`model`)

`Workload` is the parsed document. Its main fields:

| Field | Meaning |
|-------|---------|
| `params` | Resolved parameters (workload `params:` defaults plus overrides). |
| `bindings` | Workload-level Polydat bindings (`bindings:`). |
| `phases`, `phase_order` | Named `WorkloadPhase`s. `phase_order` keeps the phases in YAML declaration order. |
| `scenarios` | Scenario name to a list of `ScenarioNode`s. |
| `ops` | The top-level op pool (`ops:` / `blocks:`). |
| `report` | The parsed `report:` block (`report::Report`). |
| `stop_when` | Workload-level stop conditions. |

A `WorkloadPhase` holds the execution settings (`cycles`, `concurrency`,
`rate`, `errors`, `tries`, `timeout`, `stop_when`, `throttle`, `poll`,
`optimize`, `key_metrics`, …) and its `ops`. It gets ops either inline
(`ops:`) or by a tag selector (`tags:`), which is resolved when the workload
is parsed. `cycles`, `concurrency` and `rate` are kept as strings so they can
reference params or Polydat constants (`"{num_items}"`).

`ScenarioNode` is the authored scenario tree. Its variants include:

- `Phase(name)`
- `Comprehension { .. }`, which covers the `for_each`, `for_combinations`
  and union forms
- `DoWhile`, `DoUntil`
- `IncludedScenario`, for `scenario: <name>` reuse

`ParsedOp` is one normalized op template. It is what adapters consume:
`name`, `op` (the adapter-facing fields, `HashMap<String, serde_json::Value>`),
`params`, `bindings`, `tags`, `condition` (`if:`), `delay`, `metrics`,
`result`, `captures`, and more.

## Parsing

- `parse::parse_workload(yaml, &params)` is the in-memory entry point. It
  rejects `extends:`, because there is no file path to resolve it against.
- `parse::parse_workload_from_path(path, &params)` loads a file, follows its
  `extends:` chain (`extends::load_and_merge`), then parses the merged
  document.
- `inline::synthesize_inline_workload(op)` builds a one-op `Workload` from
  the CLI shorthand `op='hello {{cycle}}'`.

Before the YAML is parsed, `TEMPLATE(name, default)` macros are expanded
as text (`template::expand_templates`). After parsing, op templates are
instantiated and the document is checked against the construction grammar
(`construction::validate_workload`). Unknown elements on closed node kinds,
bad value forms and missing required elements are load errors, and they are
reported together with their document paths. Finally, shorthand forms are
normalized into the model.

```rust
use std::collections::HashMap;
use nmbrs_workload::model::ScenarioNode;
use nmbrs_workload::parse::parse_workload;

let yaml = r#"
scenarios:
  default:
    - schema
    - main
phases:
  schema:
    cycles: 1
    ops:
      create_table:
        stmt: "CREATE TABLE t (id int PRIMARY KEY);"
  main:
    cycles: 1000
    concurrency: 10
    rate: 500.0
    ops:
      read:
        stmt: "SELECT * FROM t WHERE id={id};"
      write:
        stmt: "INSERT INTO t (id) VALUES ({id});"
"#;

let workload = parse_workload(yaml, &HashMap::new()).unwrap();
assert_eq!(workload.phase_order, vec!["schema", "main"]);

let main = &workload.phases["main"];
assert_eq!(main.cycles.as_deref(), Some("1000"));
assert_eq!(main.rate.as_deref(), Some("500.0"));
assert_eq!(main.ops.len(), 2);

let default = &workload.scenarios["default"];
assert!(matches!(&default[0], ScenarioNode::Phase(n) if n == "schema"));
```

Other helpers:

- `bindpoints::extract_bind_points` finds `{name}` references,
  `{{expr}}` inline bindings and qualified references such as
  `{capture:foo}`.
- `tags::TagFilter` parses and applies tag selectors like `block:main`.

## Op templates (`op_templates:` / `uses:`)

A document's top-level `op_templates:` maps names to reusable op bodies,
typically a protocol request shape with an `abstract:` interface that lists
the wires it `needs`. An op instantiates one with `uses: <name>`, usually
from a workload that `extends:` a library of templates.

Instantiation happens while the document is loaded, before ops are parsed.
The op's body becomes the template's body with the op's own keys folded in:

- A key declared by both the template and the op is an error.
- `params` is the exception: the op can override it.
- `tags` are merged, and the op wins on conflicts.

The parser then checks that every `needs` wire is supplied. A template can't
itself `uses:` another template.

```yaml
# excerpt from crates/nmbrs/examples/workloads/openapi/petstore_workload.yaml
extends: ./petstore_ops.yaml

phases:
  read:
    cycles: 2
    ops:
      fetch:
        uses: getPetById
        bindings: |
          pet_id := mod(hash(cycle), 1000)
```

The library it extends
([`petstore_ops.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/openapi/petstore_ops.yaml))
declares the template:

```yaml
op_templates:
  getPetById:
    description: "Find a pet by id"
    abstract:
      needs:
        pet_id: u64
    method: GET
    uri: "{base_url}/pets/{pet_id}"
```

The constants `op_templates::OP_TEMPLATES_KEY` and `op_templates::USES_KEY`
name the two keys.

## Reports (`report`, `report_synth`)

`report::parse_report` parses a `report:` mapping into a `Report`. A `Report`
is a list of `ReportGroup`s, each holding `ReportItem`s of kind `plot`,
`table`, `text`, `file` or `details`. Styles cascade from the report defaults
to the group defaults to the item. Non-fatal problems come back as warnings
in `ParsedReport`.

When a workload has no `report:` block, `report_synth::synthesize` builds one
from phase `key_metrics` designations and scenario `anchor` cues. The result
goes through the same parser.

The `edit` module holds the lock, splice and backup primitives behind
`nmbrs report --add` / `--replace` / `rename`. It edits the YAML in place and
preserves comments and formatting.

## Example verification (`verify`)

Workloads can carry their own test rules. `nmbrs check` uses them, and so
does the test that runs every file under `crates/nmbrs/examples/`. There are two
equivalent forms. The first is `#@` comment directives, which are lines whose
first non-blank characters are `#@`:

```yaml
# from crates/nmbrs/examples/workloads/openapi/petstore_workload.yaml
#@ case renders_the_templated_requests
#@   run adapter=stdout
#@   expect http://localhost:8080/v1/pets\?limit=25
#@   expect 2 completed, 0 failed
```

| Directive | Meaning |
|-----------|---------|
| `run <args>` | CLI args for the invocation. |
| `expect <regex>` | Must match the run's combined stdout and stderr. The run must succeed. |
| `expect-fail <regex>` | The run must fail, and the regex must match its output. |
| `case <name>` | Starts a new case. Directives before any `case` form a case named `default`. |
| `again <args>` | A further invocation in the same sandbox, with session state preserved. |
| `session cwd` | Keep sessions under the sandbox working directory. |
| `timeout <secs>` | Per-case timeout. The default is 90 seconds. |
| `requires <reason>` | Skip this file, for example because it needs a live service. |

An unknown directive is a parse error.

The second form is a top-level `verify:` block, which the runtime ignores.
It can be a single directive map, a list of cases, or a map keyed by case
name. Cases from both forms are combined.

Entry points:

- `VerifyPlan::parse(src)` parses the rules.
- `verify_target`, `verify_path` and `verify_file` run workloads through an
  `nmbrs` binary and check the results.
- `check_case_output` applies the `expect` / `expect-fail` rules to output
  you already have.
- `requires_verification_rules(path)` is true for anything under an
  `examples/` directory, where rules are mandatory.

```rust
use nmbrs_workload::verify::VerifyPlan;

let src = "ops:\n  hello: \"hello {{cycle}}\"\n#@ run cycles=3\n#@ expect 3 completed, 0 failed\n";
let plan = VerifyPlan::parse(src).unwrap();
assert_eq!(plan.cases.len(), 1);
assert_eq!(plan.cases[0].name, "default");
assert_eq!(plan.cases[0].run_args, vec!["cycles=3"]);
```

See the [checking workloads guide](https://github.com/nosqlbench/nmbrs/blob/main/docs/guide/checking_workloads.md).

## Other modules

- `catalog`: the bundled-workload catalog (`lookup`, `iter`, `Tier`).
- `drivers`: driver manifests (`driver=<name>`).
- `implements`: binds an implementation workload into a blueprint's
  abstract slots.
- `magnitude`: numeric params with suffixes such as `10m` or `4Ki`.
- `suggest`: "did you mean" suggestions for workload names.
- `construction` / `vocab`: the enumerable grammar and its name lists.

## Cargo features

None.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-workload
- Example workloads: https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs/examples/workloads
- Workload contract (SRD 25):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/25_workload_contract.md
- Workload model (SRD 20):
  https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/20_workload_model.md
- Reports (SRD 46): https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/46_reports.md

## License

Apache-2.0
