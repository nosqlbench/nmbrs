# Design Memo: Binding Scope Model

## Problem

Binding scope composition currently operates on raw Polydat source strings.
There is no intermediate model that carries provenance, scope level, or
semantic identity. Every scope operation — inheritance, init injection,
auto-extern generation, shadow checking, merging — is implemented as
string scanning and concatenation. This causes:

1. **False shadow errors.** Init injection (`init var = "val"`) is
   prepended to only the first op's GkSource string. This makes
   previously-identical inherited bindings differ between ops 0 and 1+.
   The dedup check sees them as different, classifies op 1+ as
   "extra_sources", and the shadow check fires on inherited names
   that appear in both base and extras. This is a provenance failure:
   the system cannot distinguish "inherited from workload" from
   "declared at op level".

2. **String-based name extraction.** `extract_binding_names()` parses
   raw text looking for `name :=` patterns. This is a reimplementation
   of part of the Polydat parser, but without its rigor — no span tracking,
   no expression parsing, no awareness of string literals that happen
   to contain `:=`.

3. **No scope provenance.** When a name appears in a compiled GK
   program, there is no way to ask "did this come from the workload,
   the phase, the op, an auto-extern, or an init injection?" Every
   scope transformation loses provenance by flattening to a string.

4. **Fragile merge logic.** The merge step (appending extra_sources to
   base) strips `inputs`/`coordinates` by string prefix, but doesn't
   strip other duplicates. Adding init or extern stripping is whack-a-mole.

5. **Expansion pipeline order sensitivity.** `expand_polydat_bindings()` does
   param substitution, param injection, and inline expression extraction
   as string transforms on Polydat source. The order matters and the
   interactions are implicit.

## Current Architecture

```
YAML parse time
  workload bindings ──► merge_bindings() ──► phase bindings ──► op bindings
  (BindingsDef)          (string merge)       (BindingsDef)      (BindingsDef)

Executor (run_phase)
  op.bindings: String ──► substitute {var} ──► prepend "init var=val"
                            (only first op!)
               ──► generate_auto_externs()  ──► prepend "extern x: T"
                    (scans strings)               (to all ops)
               ──► expand_polydat_bindings()     ──► compile_bindings_...()
                    (string transforms)           (dedup by string eq,
                                                   shadow by name set,
                                                   merge by line append)
```

Every arrow is a string→string transformation. There is no typed model
at any intermediate step.

## Proposed Architecture: BindingScope

### Core Type

A `BindingScope` is a structured, typed representation of all the
binding contributions that go into compiling a phase's Polydat kernel. It
replaces the current approach of accumulating strings.

```rust
/// A single binding declaration with provenance.
pub struct ScopedBinding {
    /// The binding name (LHS of `:=`).
    pub name: String,
    /// The definition expression (RHS of `:=`), as Polydat source text.
    pub definition: String,
    /// Where this binding came from.
    pub origin: BindingOrigin,
    /// Modifier on the declaration (shared, final, init, none).
    pub modifier: BindingModifier,
}

/// Provenance: where a binding was declared.
pub enum BindingOrigin {
    /// Declared at workload level, inherited by this scope.
    Inherited,
    /// Declared at phase level.
    Phase,
    /// Declared at op level (augmentation).
    Op(String),  // op name
    /// Injected as an iteration variable from for_each.
    IterationVar,
    /// Generated as an auto-extern from outer scope.
    AutoExtern,
    /// Injected from workload param expansion.
    ParamExpansion,
    /// Generated from inline expression extraction.
    InlineExpr,
}

/// Typed scope for a phase compilation.
pub struct BindingScope {
    /// The coordinate declaration (e.g., "input cycle: u64").
    pub coordinates: Option<String>,
    /// All bindings, in declaration order within each origin group.
    pub bindings: Vec<ScopedBinding>,
    /// Extern declarations (name → type).
    pub externs: Vec<(String, String)>,  // (name, type_name)
}
```

### Scope Assembly Pipeline

Instead of building up a string through successive mutations, each
step contributes typed entries to a `BindingScope`:

```
1. Phase parsing ──► BindingScope with Inherited + Phase + Op entries
2. Iteration vars ──► add IterationVar entries
3. Auto-externs   ──► add AutoExtern entries (using outer ManifestEntry)
4. Param expansion──► add ParamExpansion entries
5. Inline exprs   ──► add InlineExpr entries
6. Validation     ──► scope rules checked on typed structure
7. Emission       ──► single Polydat source string for compilation
```

### Scope Rules (checked at step 6)

With provenance on every binding, scope rules become precise:

1. **No shadowing**: An `Op` binding cannot redefine a name from
   `Inherited`, `Phase`, `IterationVar`, or `AutoExtern` origin
   with a **different** definition. Same name + same definition
   from inheritance is not a shadow — it's a duplicate that gets
   deduplicated at emission.

2. **No final override**: No binding at any origin can redefine a
   name that carries `BindingModifier::Final` from an outer scope
   (`Inherited` or `AutoExtern` origin).

3. **No cross-op reference** (strict mode): An op's template cannot
   reference a name whose origin is `Op(other_op)`.

4. **Op augmentation only**: `Op`-origin bindings must introduce
   new names not present in the phase scope.

These rules are checked against structured data, not string scanning.
The error messages can include provenance context: "op 'X' binding 'Y'
(Op origin) conflicts with 'Y' (Inherited origin, defined at workload level)".

### Emission (step 7)

After validation, the scope emits a single Polydat source string by
concatenating entries in a defined order:

```
1. Coordinates declaration
2. Extern declarations
3. Init declarations (IterationVar entries)
4. Inherited bindings (deduplicated — only once per name)
5. Phase-level bindings
6. ParamExpansion bindings
7. InlineExpr bindings
8. Op-level bindings (grouped by op)
```

Each entry is emitted exactly once. Deduplication is by name: if a
name appears at multiple origins with the same definition, the highest-
precedence origin wins and others are suppressed. This eliminates the
need for line-by-line stripping during merge.

### Where BindingScope Is Constructed

The `BindingScope` is built in `run_phase()` (executor.rs), replacing
the current sequence of string mutations. The ops still carry
`BindingsDef` from parsing (this is the YAML-level representation).
The executor constructs a `BindingScope` from:

- The phase's ops' `BindingsDef` values (classified as Inherited/Phase/Op
  based on whether they match the inherited workload bindings or differ)
- The iteration variables from `for_each` bindings
- The `outer_manifest` for auto-extern generation
- The `workload_params` for param expansion

The classification "does this op have the same bindings as the phase"
is done once, structurally, before any string manipulation.

### What Changes About ParsedOp

`ParsedOp.bindings` stays as `BindingsDef` — it's the YAML-level
representation and parsing stays the same. The change is that the
executor no longer mutates op.bindings strings in-place. Instead,
it reads them once to populate `BindingScope`, then emits the final
GK source from the scope.

### What Changes About merge_bindings

`merge_bindings()` in parse.rs stays for YAML-level inheritance
(workload → phase → op). This is correct for its purpose: flattening
the YAML hierarchy into per-op `BindingsDef` values. The problem
was never with the parse-time merge — it was with the executor-time
string hacking that happens after.

### Integration Points

| Current function | Replacement |
|---|---|
| String `{var}` substitution in Polydat source | `BindingScope::add_iteration_var(name, value)` |
| `init var = "val"` prepend to first op | `ScopedBinding { origin: IterationVar, modifier: Init }` |
| `generate_auto_externs()` string scan | `BindingScope::add_externs_from_manifest(manifest, referenced)` |
| `expand_polydat_bindings()` param injection | `BindingScope::add_param_bindings(params, referenced)` |
| `extract_binding_names()` on strings | `scope.names_for_origin(origin)` or `scope.has_name(name)` |
| Shadow check via name set intersection | `scope.validate()` returns `Result<(), ScopeError>` |
| Merge by line append + strip | `scope.emit()` produces deduplicated Polydat source |

## Scope of Change

### Files Modified

| File | Change |
|---|---|
| `crates/nmbrs-runtime/src/bindings.rs` | Add `BindingScope`, `ScopedBinding`, `BindingOrigin`. Move scope assembly logic from string ops to typed construction. `compile_bindings_with_libs_excluding` accepts `BindingScope` instead of raw ops. |
| `crates/nmbrs-runtime/src/executor.rs` | `run_phase()` builds `BindingScope` instead of mutating op bindings strings. Remove init prepend, extern prepend, string dedup/shadow check. |
| `crates/nmbrs-runtime/src/runner.rs` | `generate_auto_externs()` returns `Vec<(String, String)>` (name, type) instead of a string. `expand_polydat_bindings()` returns param bindings as `Vec<ScopedBinding>` instead of mutating strings. |

### Files Not Modified

- `crates/nmbrs-workload/src/parse.rs` — `merge_bindings()` stays as-is
- `crates/nmbrs-workload/src/model.rs` — `BindingsDef`, `ParsedOp` stay as-is
- `polydat/src/dsl/*` — Polydat compiler stays as-is (it receives a string)

### Backward Compatibility

The Polydat compiler's input is unchanged (a source string). The change is
entirely in how that string is assembled. Existing workloads produce
identical Polydat source, just via a typed pipeline instead of string hacking.

## Verification

1. **Existing tests pass.** `cargo test --workspace` unchanged.
2. **Repro workload.** The false shadow error (workload-level `profiles`
   inherited by phase ops, triggering shadow on `for_each` injection)
   is resolved by provenance: both ops' `profiles` carry `Inherited`
   origin and are deduplicated, not flagged.
3. **Real shadow detected.** An op that redefines `profiles` with a
   different expression at `Op` origin is correctly flagged.
4. **Diagnostic output.** `dryrun=scope` (new mode) dumps the
   `BindingScope` showing each binding's name, definition, and origin.
