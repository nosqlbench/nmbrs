# Checking workloads — `nmbrs check`

`nmbrs check` runs a workload (or every workload under a directory) and verifies
its output against rules the workload declares. It exits non-zero on any
failure, so it drops straight into CI — and it's the same verifier the bundled
examples are tested with, so checking your own workload works exactly the way
the project checks its examples.

The target is resolved exactly the way `nmbrs run` resolves `workload=…`: a
file, a directory, or a **bundled catalog name** (`nmbrs describe workloads` /
`--all` lists them). So whatever tab-completion offers for `nmbrs check <TAB>` —
local files *and* catalog names — you can check.

```sh
nmbrs check workload=my_workload.yaml      # one file
nmbrs check examples/cursors/all_cursor/enumerate   # a bundled catalog name (runs by name)
nmbrs check nmbrs/examples/workloads/            # a whole directory (run concurrently)
```

A workload declares its verification rules in **either** of two equivalent
surfaces (a file may use one or both — their cases combine). Both are inert to
`nmbrs run`: comments are comments, and a `verify:` block is an unknown top-level
key the runtime ignores.

## 1. `#@` comment directives

Trailing comments, one directive per line:

```yaml
#@ run scenario=enumerate
#@ expect 50 completed, 0 failed
#@ case overload
#@   run concurrency=32 rate=100000
#@   expect-fail error_rate_exceeded
#@ requires backend (needs a live service to satisfy the poll predicate)
```

## 2. A `verify:` block

The same rules as structured YAML — three equivalent shapes:

```yaml
# a single case (a directive map)
verify: { run: scenario=enumerate, expect: "50 completed, 0 failed" }
```
```yaml
# a list of cases
verify:
  - { case: baseline, run: scenario=baseline, expect: "1 completed, 0 failed" }
  - { case: overload, run: "concurrency=32 rate=100000", expect-fail: error_rate_exceeded }
```
```yaml
# a name-keyed map (the key IS the case name)
verify:
  baseline: { run: scenario=baseline, expect: "1 completed, 0 failed" }
  overload: { expect-fail: error_rate_exceeded }
```

## Directives

| Directive | Meaning |
|---|---|
| `run <args>` | CLI args for this case (`scenario=…`, `key=value`, …). Default: bare. |
| `expect <regex>` | A regex that **must** match the run's combined stdout+stderr. Repeatable; in YAML, a string or a list. |
| `expect-fail <regex>` | The run must exit **non-zero** *and* match `<regex>` (for workloads that demonstrate an error). |
| `requires <reason>` | Skip this file (e.g. it needs external infrastructure); the reason is reported. |
| `timeout <secs>` | Per-case run timeout (default 90s). |
| `case <name>` | Start a new named case — a file may declare several. |

Each `nmbrs run` is launched in a sandbox working directory with a fresh
`--session-path`, so checks don't depend on prior-run state or pollute your
project. Exit code is `0` when every case passes (skips are fine), non-zero
otherwise.
