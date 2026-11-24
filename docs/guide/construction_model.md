# The Workload Construction Model

Every construct that can appear in an nmbrs workload document is
part of one enumerable, discoverable model. This page explains
what that means for you as a workload author: what the loader
will accept, how it tells you precisely what's wrong, and how
tooling can tell you what's valid *while you are still writing*.

## One model, three capabilities

The workload grammar is declared once, as a graph of **node
kinds** — the workload root, a phase, an op, a `poll:` block, a
scenario node, a stop condition, and so on (19 kinds today).
Each node kind declares its valid **elements**: the key name,
the **forms** its value may take, whether it is required, and a
one-line description. Nested constructs are direct references to
other node kinds, so the whole grammar is a single connected
graph with no side tables to fall out of date.

That one declaration powers three things:

1. **Enumeration** — every valid element of every node kind can
   be listed mechanically (this is what keeps the docs, the
   fuzzers, and the loader in agreement).
2. **Discovery** — given a *partial* document and a position in
   it, the model answers "what is valid right here?", including
   context: a phase that already carries `poll:` reports
   `concurrency` as pinned to 1; a phase with inline `ops:` no
   longer offers the `tags:` selector; an op declared
   `abstract:` stops offering a statement payload.
3. **Validation** — any document, partial or complete, can be
   checked against the model. This is the enforcement gate the
   loader runs.

## What the loader enforces

When a workload is loaded, after `extends:` merging and before
anything executes, the document is validated against the model.
Three kinds of findings, each reported with the exact document
path:

- **Unknown element on a closed node.** Every node kind is
  closed unless it *declares* an open surface (see below). A
  typo like `cylces:` on a phase fails the load naming
  `workload.phases.<name>.cylces`, instead of being silently
  ignored.
- **Value form violations.** Each element declares the forms its
  value may take — `cycles` accepts an unsigned integer or a
  `{param}` reference; `checkpoint` accepts `idempotent`,
  `disabled`, or its map form; a metric `kind` must be `gauge`
  or `counter`. A value matching none of its declared forms
  fails the load with the form list in the message.
- **Missing required elements.** A `poll:` block without
  `until:`, a `relevancy:` block without `actual:`/`expected:` —
  required elements are enforced for documents being loaded for
  execution. (Tooling validating a document you are still
  editing uses partial mode, which skips this check.)

All violations are collected and reported together — you fix the
list, not one error per attempt.

## The forms vocabulary

Element values are described by a small set of forms you will
see named in error messages:

| Form | Meaning |
|---|---|
| `bool`, `u64`, `f64`, `str`, `StrList` | plain scalars / string lists (numeric strings accepted where documented). `bool` is a YAML bool or the string `"true"` / `"false"` — never `on`/`off`, `yes`/`no`, or `1`/`0` |
| `Duration` | `"2.5h"`, bare seconds, or a `{param}` reference |
| `ParamRef` | a `{param}` / iteration-variable reference is accepted here |
| `GkSource` / `GkExpr` | a polydat bindings block / a single polydat expression |
| `PathExpr` | a capture/result path (`/0/field`, `rows[*].col`) |
| `MetricSelector` | `"family, label=value"` |
| `Vocab(...)` | one of a closed set of words |
| node references | a nested construct, a named map of them, or a list of them |
| `MapOf(...)` / `ListOfForm(...)` | maps/lists whose *values* all take one form (e.g. every `capture:` value is a `PathExpr`) |

The grammar *inside* `GkSource`/`GkExpr` values belongs to the
polydat compiler, which has its own registry and diagnostics —
the construction model checks that the right *kind* of content
is in the right place; the compiler proves the content.

## Declared open surfaces

Openness is always declared, never implied. Exactly these
surfaces accept keys beyond the enumerated set:

- **Op adapter payloads.** Statement-adjacent fields belong to
  the adapter (`prepared:`, `uri:`, `method:`, …). Adapters with
  a closed op-field vocabulary (http, testkit) reject unknown
  fields themselves; the CQL adapter's payload surface is open
  by design.
- **Scenario legacy command strings.** A scenario entry of the
  form `name: "run …"` (string value) is the legacy inline
  command shape and passes through; *structural* scenario keys
  are closed and checked.
- **Free maps by declaration** — `params:`, `set:`, `tags:`,
  the `report:` block (whose directive grammar has its own
  registry), and similar author-keyed maps.

## The coherence axiom

> **If a construction cannot be represented in the fuzzer, it is
> not yet a valid construction. Any intended valid construction
> must be implemented to be discoverable stochastically by
> synthetic fuzzing.**

Validity is not a documentation claim — it is demonstrated by
the synthesizer being able to generate the construction and the
system handling it cleanly. The fuzzer's explicit deferral list
is therefore a *debt register*: every entry names a construction
that is enumerated but not yet fuzz-discoverable, and the
coverage test keeps that list honest in both directions (nothing
may sit on it once covered; nothing may be missing from both the
generator and the list).

## How the model stays honest

Two mechanical guards keep the declaration from drifting:

- **Struct pins.** Tables for serde-backed constructs are
  checked against the model structs themselves: a field probe
  extracts the struct's exact field list (aliases included) and
  the test fails if the table disagrees. Adding a field to
  `WorkloadPhase` without updating its table fails the build.
- **Fuzz coverage lock.** The workload synthesizer's coverage
  test fails whenever the model gains a construct that the fuzz
  generator neither produces nor has consciously deferred with a
  written reason — new constructs enter the fuzz surface by
  default.

## For tooling integrators

The model is a library surface in `nmbrs-workload`:

- `construction::root()` / `construction::ALL_NODES` — the node
  graph and its enumeration.
- `construction::discover_at(partial_ast, path)` — node kind and
  valid elements at any position of a partial document, with
  context narrowing applied.
- `construction::validate_workload(ast, Mode::Partial |
  Mode::Complete)` — the same check the loader runs, with
  partial mode for in-progress documents.
- `vocab::*` — flat name projections of the tables, for
  consumers that only need the vocabulary.
