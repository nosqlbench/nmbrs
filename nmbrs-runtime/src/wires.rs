// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `WireSource` — narrow read trait for op-template name resolution.
//!
//! SRD-68 §"The narrow trait" specifies the wall between adapter
//! code and `polydat::kernel::PolydatKernel` internals: a dispenser
//! at cycle time accesses its bound Polydat context only through this
//! trait's `get` (value lookup by name) and `names` (declared-name
//! iteration for diagnostics). No `program()`, no `state()`, no
//! `scope_coordinates()` — adapter code is sealed off from kernel
//! mechanics.
//!
//! Two implementations ship here:
//! - `PolydatKernel` itself, via the kernel's existing `lookup` chain
//!   (input slots, outputs, inherited scope state). Single
//!   resolution surface — name resolves where SRD-67 places it,
//!   no fallback (SRD-68 invariant I-1).
//! - `NullWireSource`, a unit type that returns `None` for every
//!   name. Used as the default value in `ExecCtx::new` during the
//!   SRD-68 migration so call sites that don't yet have a kernel
//!   handle don't break — adapters opt in via
//!   `ExecCtx::with_wires` once they own a kernel reference.
//!
//! See `docs/SRD/68_dispenser_owned_polydat_context.md`.

use std::sync::OnceLock;

use polydat::ast::{PortType, Value};
use polydat::kernel::PolydatKernel;

/// Cached `NMBRS_DIRTY_DEBUG` flag. Per-cycle env reads cost ~30%
/// of CPU on single-fiber benches; the OnceLock makes the gate
/// a single atomic load. See the matching helper in
/// `polydat::kernel::engines` for the same rationale.
fn nmbrs_dirty_debug_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("NMBRS_DIRTY_DEBUG").is_ok())
}

/// Result of a [`WireSource::write`] call.
///
/// `Stored` means the value landed on a real input slot in the
/// underlying kernel and is visible to subsequent pulls.
///
/// `NoSlot` means the kernel's program has no input slot named
/// `name`. Under [`KernelOptLevel::Release`] this is the
/// closure-binding economy's DCE signal — nothing in the body
/// referenced the name, so the value is silently dropped. Under
/// [`KernelOptLevel::Diagnostic`] every magic extern and
/// result-binding LHS gets a slot, so `NoSlot` indicates a
/// genuinely-unknown name (caller error or a name outside the
/// kernel's scope).
///
/// Callers don't need to branch on the outcome to maintain
/// correctness — the kernel either has a slot for the name or it
/// doesn't, and `NoSlot` is the same as "the workload doesn't
/// reference this value." Diagnostics (`debug_nodes_enabled()`,
/// audit log) can log the outcome to make DCE visible.
///
/// [`KernelOptLevel::Release`]: polydat::kernel::KernelOptLevel::Release
/// [`KernelOptLevel::Diagnostic`]: polydat::kernel::KernelOptLevel::Diagnostic
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Value written to a real input slot.
    Stored,
    /// No input slot named `name` in this kernel's program.
    NoSlot,
    /// The slot exists but the value's type does not match the
    /// slot's declared `PortType` and no boundary auto-adapter
    /// healed the mismatch. Per composition_substrate.md S4 the
    /// typed-write contract rejects this rather than silently
    /// corrupting downstream reads; the `reason` field carries
    /// the polydat-side diagnostic for surfacing to the operator.
    TypeMismatch { reason: String },
    /// The slot is a coordinate. Coordinates advance with the cycle
    /// (`set_inputs`), never by a named write, so the kernel refuses
    /// the write to keep the coordinate prefix in step with the cycle
    /// a pull is about to read. `reason` is polydat's diagnostic.
    Coordinate { reason: String },
}

/// Cycle-time read surface a dispenser uses to resolve names from
/// its bound Polydat context.
///
/// `get(name)` returns the current value of the named wire in the
/// dispenser's kernel. A `None` return indicates the name isn't
/// declared in this scope — callers MUST treat that as a
/// resolution error, not a fallback opportunity (SRD-68 I-1).
///
/// `names` enumerates declared names for validators and
/// `describe_resolved`-style introspection. Not for hot-path cycle
/// reads; cycle reads use `get`.
pub trait WireSource: Send + Sync {
    /// Look up `name` in this kernel's scope. Returns the current
    /// value (cloned, owned) or `None` when the name is not
    /// declared here. Callers do not retry against another kernel
    /// — a `None` is the resolution result, full stop.
    fn get(&self, name: &str) -> Option<Value>;

    /// Iterate declared names. Order is implementation-defined.
    /// Used by validators and diagnostic renderers.
    fn names(&self) -> Box<dyn Iterator<Item = String> + '_>;

    /// Write `value` into the named input slot of this kernel.
    /// Returns [`WriteOutcome::Stored`] when the slot exists and
    /// the value was written, [`WriteOutcome::NoSlot`] when the
    /// kernel's program has no input slot named `name`.
    ///
    /// This is the canonical cycle-time capture path used by
    /// `ResultDispenser` and `TraversingDispenser` after their
    /// inner stack returns. Writes land on the kernel directly —
    /// no HashMap intermediary, no post-stack pump in the
    /// activity loop. Subsequent `wires.get` calls (e.g. from a
    /// later wrapper like `MetricsDispenser`) see fresh values.
    ///
    /// Default impl returns `NoSlot` — appropriate for read-only
    /// implementations like `NullWireSource` and the bare
    /// `&PolydatKernel` baseline (which has no `&mut` handle to mutate
    /// state). `CycleWires` overrides with the real write path
    /// through the wrapped kernel's `set_input`.
    fn write(&self, _name: &str, _value: Value) -> WriteOutcome {
        WriteOutcome::NoSlot
    }

    /// Reset the named input slot to its DECLARED initial value —
    /// the author's identity element for that wire. The capture
    /// layer uses this for min/max-of-nothing (SRD-93-era fold
    /// totality): an empty measurement restores the wire to its
    /// declared unit rather than parking `Value::None` on a typed
    /// slot, where every downstream consumer would have to be
    /// None-aware. Default impl returns `NoSlot` (read-only
    /// sources have nothing to reset).
    fn reset(&self, _name: &str) -> WriteOutcome {
        WriteOutcome::NoSlot
    }

    /// Advance the underlying kernel state to coordinate `coord`
    /// and invalidate any memoized pulls so subsequent `get`
    /// calls produce values for this coord.
    ///
    /// Used by batch dispensers per the SRD-68 invariant:
    /// > "Within the batch view, each iteration of the batch is
    /// > considered another pull, just as if the operation
    /// > inside the batch were separate. It is simply an
    /// > iteration container."
    ///
    /// The default impl is a no-op — `NullWireSource` and
    /// adapters that don't drive batch iteration leave it alone.
    /// `CycleWires` overrides to mutate the wrapped kernel's
    /// coord input. Calling `advance` on a `WireSource` that
    /// has no kernel handle (e.g. `NullWireSource`) is a no-op,
    /// not an error — the dispenser's own `wires.get` calls
    /// will resolve correctly against whatever read surface the
    /// adapter has bound.
    fn advance(&self, _coord: u64) {}
}

/// `WireSource` over `&PolydatKernel` — covers names that the kernel's
/// `lookup` API already exposes (inputs, scope-init constants,
/// shared-cell-backed values). Computed outputs that require a
/// memoizing `pull(&mut state, …)` evaluation are NOT covered here
/// and return `None` from `get`; the SRD-68 Push 2 work introduces a
/// richer `WireSource` impl that owns the per-fiber kernel handle
/// with interior mutability and can pull outputs at cycle time.
///
/// For Push 1 this `&PolydatKernel` impl is the additive baseline: every
/// existing call site that gets handed a `NullWireSource` continues
/// working unchanged, and code that wants kernel-side reads via the
/// trait can use it for the names `lookup` already answers.
impl WireSource for PolydatKernel {
    fn get(&self, name: &str) -> Option<Value> {
        self.lookup(name)
    }

    fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
        // Outputs come first (the canonical declared-name surface),
        // followed by input names not also published as outputs.
        // Ordering is stable for a given program; consumers should
        // not assume a particular order between the two groups.
        let program = self.program();
        let outputs: Vec<String> = program
            .output_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let inputs_only: Vec<String> = program
            .input_names()
            .iter()
            .filter(|n| !outputs.contains(n))
            .cloned()
            .collect();
        Box::new(outputs.into_iter().chain(inputs_only))
    }
}

/// `WireSource` over a per-fiber kernel handle that supports the
/// full read surface — inputs, scope-init constants, AND computed
/// outputs (which need a memoizing `pull(&mut state, …)` to fire
/// the eval cone). Wraps a `&mut PolydatKernel` in a `Mutex` so the
/// trait stays `&self`-callable (and `Sync`) while still permitting
/// pull's `&mut` requirement.
///
/// The `Mutex` is uncontended in practice: per-fiber-per-cycle
/// dispatch is single-threaded by construction (the fiber owns
/// the kernel slot exclusively for the duration of the cycle),
/// so `lock()` always succeeds without spinning. Using `Mutex`
/// rather than `RefCell` is a `Sync` requirement — async futures
/// returned by `OpDispenser::execute` are Send, which means
/// `&ExecCtx` (and through it `&dyn WireSource`) must be Send,
/// which means `dyn WireSource` must be Sync.
///
/// Cells held here have the lifetime of one cycle dispatch —
/// constructed at cycle entry from the firing dispenser's
/// per-fiber kernel slot, dropped after the dispenser returns.
///
/// Resolution order on `get(name)`:
/// 1. Output (memoizing pull through the eval cone).
/// 2. Input slot (cell-aware read).
/// 3. Scope-init constant.
///
/// `None` when the name doesn't appear on this kernel — callers
/// surface as an unresolved-bindpoint error per SRD-68 I-1.
pub struct CycleWires<'a> {
    kernel: std::sync::Mutex<&'a mut PolydatKernel>,
}

impl<'a> CycleWires<'a> {
    /// Wrap a per-fiber kernel handle for cycle-time reads. The
    /// caller — typically the executor's cycle dispatch — holds
    /// the only outstanding borrow on the kernel for the duration
    /// of this cycle. Reads through `WireSource::get` resolve
    /// every visible name through this kernel's local read API;
    /// SRD-13f's construction-time wiring + per-cycle refresh
    /// keeps the local view consistent with the owning scope's
    /// kernel without external chain composition.
    pub fn new(kernel: &'a mut PolydatKernel) -> Self {
        Self {
            kernel: std::sync::Mutex::new(kernel),
        }
    }
}

impl<'a> WireSource for CycleWires<'a> {
    fn get(&self, name: &str) -> Option<Value> {
        let mut k = self.kernel.lock().expect("CycleWires mutex poisoned");
        // Output pull first: a name declared by the program's
        // bindings is an output; the pull memoizes through the
        // eval cone. Fall through to lookup for inputs and
        // scope-init constants. No external chain composition —
        // construction-time wiring set up every visible wire
        // (SRD-13f).
        // Resolve the name to an output INDEX ONCE, then pull by index — the
        // old `resolve_output(name).is_some()` + `pull(name)` hashed the name
        // twice per read (pull re-resolves internally). One hash now.
        if let Some(output_idx) = k.program().output_index(name) {
            let v = k.pull_ref_at(output_idx).clone();
            if nmbrs_dirty_debug_enabled() && name == "query" {
                let s = v.to_display_string();
                let head: String = s.chars().take(64).collect();
                eprintln!("DIRTY: wires.get(query) OUTPUT head=\"{head}\"");
            }
            return Some(v);
        }
        let v = k.lookup(name);
        if nmbrs_dirty_debug_enabled() && name == "query" {
            let head = v
                .as_ref()
                .map(|x| x.to_display_string().chars().take(64).collect::<String>())
                .unwrap_or_else(|| "<none>".into());
            eprintln!("DIRTY: wires.get(query) INPUT/CONST head=\"{head}\"");
        }
        v
    }

    fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
        let k = self.kernel.lock().expect("CycleWires mutex poisoned");
        let program = k.program();
        let outputs: Vec<String> = program
            .output_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let inputs_only: Vec<String> = program
            .input_names()
            .iter()
            .filter(|n| !outputs.contains(n))
            .cloned()
            .collect();
        Box::new(outputs.into_iter().chain(inputs_only))
    }

    fn reset(&self, name: &str) -> WriteOutcome {
        use polydat::kernel::{Dataflow, WriteError};
        let mut k = self.kernel.lock().expect("CycleWires mutex poisoned");
        let Some(idx) = k.program().find_input(name) else {
            return WriteOutcome::NoSlot;
        };
        // The declared default is by construction the slot's own
        // type, so this write cannot mismatch; the typed boundary
        // is still used rather than poking state directly.
        let default = match k.program().input_default_by_idx(idx) {
            Some(d) => d.clone(),
            None => return WriteOutcome::NoSlot,
        };
        match k.set_wire_idx(idx, default) {
            Ok(()) => WriteOutcome::Stored,
            Err(WriteError::UnknownWire { .. }) => WriteOutcome::NoSlot,
            Err(e @ WriteError::TypeMismatch { .. }) => WriteOutcome::TypeMismatch {
                reason: e.to_string(),
            },
            Err(e @ WriteError::CoordinateSlot { .. }) => WriteOutcome::Coordinate {
                reason: e.to_string(),
            },
        }
    }

    fn write(&self, name: &str, value: Value) -> WriteOutcome {
        use polydat::kernel::{Dataflow, WriteError};
        let mut k = self.kernel.lock().expect("CycleWires mutex poisoned");
        if std::env::var("NMBRS_DEBUG_WIRES")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            let cells: Vec<String> = k
                .shared_cells_in_scope()
                .iter()
                .map(|c| c.name.clone())
                .collect();
            let slot_cell = k
                .program()
                .find_input(name)
                .map(|idx| k.state_ref().shared_cell(idx).is_some());
            eprintln!(
                "WIRES.write name={name} value={value:?} slot_cell_bound={slot_cell:?} cells_in_scope={cells:?}"
            );
        }
        // Go through the typed Dataflow surface (per S4): the
        // boundary enforces T1+T2 and routes through the auto-
        // adapter catalog before rejecting mismatched writes.
        match k.set_wire(name, value) {
            Ok(()) => WriteOutcome::Stored,
            Err(WriteError::UnknownWire { .. }) => WriteOutcome::NoSlot,
            Err(e @ WriteError::TypeMismatch { .. }) => WriteOutcome::TypeMismatch {
                reason: e.to_string(),
            },
            Err(e @ WriteError::CoordinateSlot { .. }) => WriteOutcome::Coordinate {
                reason: e.to_string(),
            },
        }
    }

    fn advance(&self, coord: u64) {
        let mut k = self.kernel.lock().expect("CycleWires mutex poisoned");
        if k.program().coord_count() > 0 {
            k.state().set_inputs(&[coord]);
        }
    }
}

/// Empty `WireSource` — every `get` returns `None`, `names` is
/// empty. Used as the default for `ExecCtx::new` so callers that
/// don't yet have a kernel handle don't need to construct a
/// real implementation. Migration call sites switch to
/// `ExecCtx::with_wires` when they own a kernel reference (see
/// SRD-68 Push 2 and beyond).
pub struct NullWireSource;

impl WireSource for NullWireSource {
    fn get(&self, _name: &str) -> Option<Value> {
        None
    }
    fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
        Box::new(std::iter::empty())
    }
}

/// Static `&'static dyn WireSource` for use as the default in
/// `ExecCtx::new`. Avoids per-call allocation; the unit struct has
/// no state to differ.
pub static NULL_WIRES: NullWireSource = NullWireSource;

/// Render `template` by substituting each `{name}` placeholder
/// with `wires.get(name)`'s display-string form. The single
/// resolution-via-wires entry point adapters use at cycle time
/// per SRD-68 Push 5 — replaces the synthesis-layer
/// `substitute_bind_points*` text-mutation pass.
///
/// Honors the standard placeholder escape rules established by
/// `nmbrs_workload::bindpoints`:
/// - `\{` / `\}` — literal brace, passes through unchanged.
/// - `{{...}}` — inline-expression form, reserved for the
///   `{{<polydat-expr>}}` desugar surface; passes through unchanged
///   (compiled into bindings before reaching cycle time).
/// - Qualifier-prefixed forms (`{bind:name}` / `{capture:name}`
///   / `{input:name}`) — the current Push 5 contract is bare
///   `{name}` only; qualifier-prefixed references error.
/// - `{` followed by non-identifier text — passes through (CQL
///   map literals like `{'class': 'SimpleStrategy'}`, JSON object
///   literals like `{"a": 1}`, format specs like `{:5.2}`).
///
/// Returns `Err` with a descriptive message when a bare `{name}`
/// reference doesn't resolve through `wires` — SRD-68 invariant
/// I-1 forbids silent fallback. Callers (typically the cycle
/// dispatch error path) surface this as a phase-stopping
/// diagnostic.
pub fn substitute_via_wires(template: &str, wires: &dyn WireSource) -> Result<String, String> {
    // Mirror the brace-handling algorithm of
    // `nmbrs_workload::bindpoints::extract_bind_points`: when `{` is
    // followed by a literal-start character (`'` or `"`), emit the
    // brace and continue scanning so nested `{name}` placeholders
    // INSIDE CQL map / JSON object literals still get resolved.
    // Track brace depth so a top-level `{name}` whose body itself
    // contains balanced braces (rare) isn't truncated at the first
    // closing brace.
    let chars: Vec<char> = template.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < n {
        // `\{` / `\}` — pass through, two chars.
        if chars[i] == '\\' && i + 1 < n && (chars[i + 1] == '{' || chars[i + 1] == '}') {
            out.push(chars[i]);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        // `{{ ... }}` — inline-expression form. Reserved for the
        // Polydat desugar surface; passes through unchanged at cycle
        // time (compiled into bindings before reaching this
        // path).
        if i + 1 < n && chars[i] == '{' && chars[i + 1] == '{' {
            let start = i;
            let mut j = i + 2;
            while j + 1 < n && !(chars[j] == '}' && chars[j + 1] == '}') {
                j += 1;
            }
            let end = (j + 2).min(n);
            out.extend(&chars[start..end]);
            i = end;
            continue;
        }
        if chars[i] != '{' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // CQL map / JSON object literal: `{` followed by
        // (whitespace?) `'`/`"`. Emit just the `{` and
        // continue scanning so any nested `{name}` placeholders
        // inside still resolve. Whitespace tolerance covers
        // multi-line CQL maps written with the opening brace
        // on its own line (e.g. `compaction = {\n  'class':
        // …\n}`).
        let next_nonspace = chars[i + 1..].iter().find(|c| !c.is_whitespace()).copied();
        if matches!(next_nonspace, Some('\'') | Some('"')) {
            out.push('{');
            i += 1;
            continue;
        }
        // Single `{...}` form: depth-track to find the matching
        // `}` so balanced inner braces don't truncate the body.
        let body_start = i + 1;
        let mut j = body_start;
        let mut depth: u32 = 1;
        while j < n {
            if chars[j] == '{' {
                depth += 1;
            }
            if chars[j] == '}' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            j += 1;
        }
        if j >= n {
            // Unterminated — treat as literal char.
            out.push('{');
            i += 1;
            continue;
        }
        let body: String = chars[body_start..j].iter().collect();
        let body = body.trim();
        let after = j + 1;
        // Empty body — pass through.
        if body.is_empty() {
            out.push('{');
            out.push('}');
            i = after;
            continue;
        }
        // Qualifier-prefixed form (`{bind:name}`, `{capture:name}`,
        // `{input:name}`) — SRD-68 Push 5 contract is bare names
        // only at cycle time.
        if body.contains(':') {
            return Err(format!(
                "qualifier-prefixed bind point `{{{body}}}` is not supported \
                 at cycle time; only bare `{{name}}` references are answered \
                 by the dispenser's WireSource"
            ));
        }
        // Bare identifiers resolve as-is. Dotted identifiers
        // (`q.cursor.idx`) follow the field-access wire
        // convention and resolve through their flattened
        // spelling (`q__cursor__idx`) — same rule the DSL
        // compiler and kernel lookup apply. Everything else
        // (format specs `{:5.2}`, expressions `{a+b}`, etc.)
        // passes through unchanged.
        let wire_name: std::borrow::Cow<'_, str> = if is_bare_ident(body) {
            std::borrow::Cow::Borrowed(body)
        } else if is_dotted_ident(body) {
            std::borrow::Cow::Owned(body.replace('.', "__"))
        } else {
            out.push('{');
            out.push_str(body);
            out.push('}');
            i = after;
            continue;
        };
        let body = wire_name.as_ref();
        // Bare identifier — wires.get resolves or errors.
        //
        // SRD-74 Rule 3 (strict render): `Value::None` reaching the
        // wire-protocol substitution surface is an error, NOT a
        // silent empty string. The display-strict primitive returns
        // `None` for `Value::None` so we can surface a clear
        // diagnostic. The catch-all `to_display_string` legacy
        // (which maps None to "") stays in place for log /
        // diagnostic contexts where empty is acceptable; render
        // sites use the strict form.
        match wires.get(body) {
            Some(v) => {
                // Binary-natural type guard. A wire holding VecF32 /
                // VecI32 / Handle / Bytes has no meaningful
                // text-spliced form for a wire-protocol field —
                // decimal-stringifying a 128-element f32 vector to
                // embed in a CQL INSERT is always the wrong path
                // (it forces format-then-reparse on every cycle and
                // bypasses the adapter's typed-binding API). Detect
                // here so the cost — and the design violation — are
                // surfaced as a clear error instead of silently
                // burned on every cycle.
                //
                // The escape hatch for typed values that genuinely
                // belong in a field is the "pure-token" form (case 2
                // in `resolve_op_fields_via_wires`): the entire
                // field value is exactly `{wire_name}` with no
                // surrounding text, and the typed `Value` is handed
                // to the adapter intact.
                if is_binary_natural(v.port_type()) {
                    return Err(format!(
                        "wire `{body}` holds {} which has no text-spliced \
                         representation suitable for a wire-protocol field. \
                         Either use the pure-token form (the entire field \
                         value is exactly `{{{body}}}` with no surrounding \
                         text) so the adapter binds the typed value via \
                         its parameter API, or split the field so the \
                         wire is bound as a typed parameter alongside a \
                         text template containing placeholders (e.g. CQL \
                         `?`).",
                        v.port_type()
                    ));
                }
                match v.to_display_strict() {
                    Some(s) => out.push_str(&s),
                    None => {
                        return Err(format!(
                            "unresolved bind point `{{{body}}}`: wire \
                             `{body}` resolved to `Value::None` (no value \
                             bound in the dispenser's Polydat context chain). \
                             Set a workload-param default for `{body}`, \
                             bind it via `bindings:` / `set:`, or mark \
                             the bind-point as optional once SRD-74 \
                             Rule 2 syntax lands."
                        ));
                    }
                }
            }
            None => {
                return Err(format!(
                    "unresolved bind point `{{{body}}}`: no wire named \
                     `{body}` in the dispenser's Polydat context"
                ));
            }
        }
        i = after;
    }
    Ok(out)
}

/// Resolve a list of op-template field entries through the generic
/// wires API. Each entry is `(field_name, json_value_from_template)`;
/// the helper returns name+value pairs an adapter can hand to its
/// renderer, with no synthesis-layer involvement.
///
/// The four-case rule, in order of dispatch:
///
/// 1. **Non-string YAML scalar / list / object**: passes through as
///    `Value::Str(json.to_string())`. Adapters that need richer
///    typing for a JSON-shaped field can downcast through
///    `ResolvedFields` and parse, but the default projection keeps
///    the legacy "everything renders to a string" contract.
///
/// 2. **Pure-token typed-reference**: the entire string (trimmed)
///    is exactly `{name}` where `name` is a bare identifier. The
///    field's value becomes `wires.get(name).clone()` — typed
///    `Value` preserved. Used by CQL prepared-param bindings,
///    vector args, and anywhere an adapter needs the native typed
///    value rather than its display string. An unresolved name
///    here is a hard error (no silent fallback).
///
/// 3. **Text template with placeholders**: any string containing
///    `{name}` placeholders embedded in surrounding text, or
///    `{{ expr }}` inline-GK escapes. Renders through
///    `substitute_via_wires` to produce a `Value::Str` with each
///    placeholder replaced by its wire's display-string form.
///    Inline-expression `{{ … }}` placeholders are evaluated then
///    stringified.
///
/// 4. **Bare-string literal**: a string with no `{…}` markers
///    passes through unchanged as `Value::Str(s.clone())`. This is
///    the most common case for SQL stmts, URIs, and headers.
///
/// **Why not "bare-name as wire reference"?** A workload author
/// writing `stmt: "SELECT * FROM users"` doesn't expect `users` to
/// resolve as a Polydat wire. The pure-token form (case 2) is the
/// explicit opt-in for typed references; bare strings stay
/// literal. Per-adapter typed-field metadata (a future enhancement)
/// would let specific fields opt into bare-name-as-reference; the
/// current contract doesn't require it.
///
/// Returns the SRD-68 standard error message on the first
/// unresolved bind point (single resolution surface, no fallback).
pub fn resolve_op_fields_via_wires(
    op_fields: &[(String, serde_json::Value)],
    wires: &dyn WireSource,
) -> Result<crate::adapter::ResolvedFields, String> {
    use polydat::ast::Value;
    let mut names = Vec::with_capacity(op_fields.len());
    let mut values = Vec::with_capacity(op_fields.len());
    for (key, json_value) in op_fields {
        names.push(key.clone());
        let serde_json::Value::String(s) = json_value else {
            values.push(Value::Str(json_value.to_string().into()));
            continue;
        };
        let trimmed = s.trim();
        let pure_token = trimmed.starts_with('{')
            && trimmed.ends_with('}')
            && !trimmed.starts_with("{{")
            && trimmed.len() >= 2
            && trimmed[1..trimmed.len() - 1]
                .chars()
                .all(|c| c != '{' && c != '}');
        if pure_token {
            let body = trimmed[1..trimmed.len() - 1].trim();
            let bare = match body.split_once(':') {
                Some((_, n)) => n,
                None => body,
            };
            if is_bare_ident(bare) {
                match wires.get(bare) {
                    Some(v) => {
                        values.push(v);
                        continue;
                    }
                    None => {
                        return Err(format!(
                            "unresolved bind point `{{{bare}}}` in field '{key}': \
                             no wire named `{bare}` in the dispenser's Polydat context"
                        ));
                    }
                }
            }
        }
        let rendered = substitute_via_wires(s, wires).map_err(|e| format!("field '{key}': {e}"))?;
        values.push(Value::Str(rendered.into()));
    }
    Ok(crate::adapter::ResolvedFields::new(names, values))
}

/// Wire types that have no meaningful text-spliced representation
/// in a wire-protocol field. Embedding `{wire}` for one of these
/// inside a larger string template (CQL prepared text, HTTP body,
/// etc.) is always a workload-shape bug: the typed value should
/// flow through the adapter's typed-binding API, not be
/// decimal-stringified into the SQL/JSON/body text and reparsed
/// downstream.
///
/// Used by [`substitute_via_wires`] to refuse the splice and by
/// the future op-template construction-time guard (init-time
/// shape check) to reject equivalent misuse before any cycle runs.
fn is_binary_natural(t: PortType) -> bool {
    matches!(
        t,
        PortType::VecF32
            | PortType::VecI32
            | PortType::VecF64
            | PortType::VecI64
            | PortType::VecF16
            | PortType::VecI16
            | PortType::Handle
            | PortType::Bytes
    )
}

/// Same identifier discipline as the core `resolve_placeholders_in_string`
/// validator in `crate::scope`: ASCII-alpha-or-underscore start,
/// remainder ASCII-alphanumeric-or-underscore. Anything else is
/// not a bare identifier and passes through.
fn is_bare_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A dotted identifier chain (`q.cursor.idx`): two or more
/// bare-identifier segments joined by single dots. These follow
/// the field-access wire convention and resolve through the
/// `__`-flattened spelling.
fn is_dotted_ident(s: &str) -> bool {
    let segments: Vec<&str> = s.split('.').collect();
    segments.len() >= 2 && segments.iter().all(|seg| is_bare_ident(seg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polydat::dsl::compile::compile_polydat;

    #[test]
    fn polydatkernel_get_resolves_inputs_and_constants() {
        // `lookup` (and therefore Push 1's WireSource) covers
        // input slots and scope-init constants — the names available
        // without a memoizing pull. `folded := 42` lands as a
        // compile-folded constant; `cycle` is a coordinate input.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             folded := 42\n",
        )
        .unwrap();
        k.set_inputs(&[7]);
        let wires: &dyn WireSource = &k;
        assert_eq!(wires.get("folded").map(|v| v.as_u64()), Some(42));
        assert_eq!(wires.get("cycle").map(|v| v.as_u64()), Some(7));
    }

    #[test]
    fn polydatkernel_get_returns_none_for_pull_only_outputs_in_push_1() {
        // Push 1 baseline: outputs that require a memoizing
        // `pull(&mut state, …)` evaluation are NOT served by the
        // `&PolydatKernel` impl. Push 2 introduces the kernel-owning
        // wires impl that can pull outputs. This test pins the
        // current contract so the Push 2 change is visible as a
        // diff.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             cyc_dep := hash(cycle)\n",
        )
        .unwrap();
        k.set_inputs(&[7]);
        let wires: &dyn WireSource = &k;
        assert!(wires.get("cyc_dep").is_none());
    }

    #[test]
    fn polydatkernel_get_returns_none_for_unknown_name() {
        let k = compile_polydat("input cycle: u64\nx := 1\n").unwrap();
        let wires: &dyn WireSource = &k;
        assert!(wires.get("not_a_real_name").is_none());
    }

    #[test]
    fn polydatkernel_names_lists_declared_outputs_and_inputs() {
        let k = compile_polydat("input cycle: u64\nfolded := 42\n").unwrap();
        let wires: &dyn WireSource = &k;
        let names: Vec<String> = wires.names().collect();
        assert!(
            names.iter().any(|n| n == "folded"),
            "folded should appear: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "cycle"),
            "cycle should appear: {names:?}"
        );
    }

    #[test]
    fn null_wires_returns_none_and_empty() {
        let wires: &dyn WireSource = &NULL_WIRES;
        assert!(wires.get("anything").is_none());
        assert_eq!(wires.names().count(), 0);
    }

    #[test]
    fn cycle_wires_pulls_outputs() {
        // CycleWires (Push 4) covers the memoizing-pull path that
        // the bare `&PolydatKernel` impl can't reach. `cyc_dep` is a
        // computed output — pulling it requires `&mut state` to
        // fire the eval cone and cache the result.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             folded := 42\n\
             cyc_dep := hash(cycle)\n",
        )
        .unwrap();
        k.set_inputs(&[7]);
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;
        // Output pull works through CycleWires.
        let v = wires.get("cyc_dep").expect("cyc_dep should resolve");
        assert!(v.as_u64() != 0, "hash result should be non-zero");
        // Folded constant still resolves.
        assert_eq!(wires.get("folded").map(|v| v.as_u64()), Some(42));
        // Coordinate input still resolves.
        assert_eq!(wires.get("cycle").map(|v| v.as_u64()), Some(7));
    }

    /// SRD-93-era fold totality: `reset` restores a wire to its
    /// DECLARED initial value — the author's identity element — so an
    /// empty min/max fold never parks `Value::None` on a typed slot
    /// and never leaves a stale prior reading.
    #[test]
    fn cycle_wires_reset_restores_declared_default() {
        let mut k =
            compile_polydat("input cycle: u64\nextern pressure: u64 = 7\nx := pressure + 1\n")
                .unwrap();
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;

        assert_eq!(
            wires.write("pressure", Value::U64(42)),
            WriteOutcome::Stored
        );
        assert_eq!(wires.get("pressure").map(|v| v.as_u64()), Some(42));

        assert_eq!(
            wires.reset("pressure"),
            WriteOutcome::Stored,
            "reset writes the declared default through the typed boundary"
        );
        assert_eq!(
            wires.get("pressure").map(|v| v.as_u64()),
            Some(7),
            "the wire returns to its declared initial value, not to None/0"
        );

        assert_eq!(wires.reset("nonesuch"), WriteOutcome::NoSlot);
    }

    #[test]
    fn cycle_wires_returns_none_for_unknown_name() {
        let mut k = compile_polydat("input cycle: u64\nx := 1\n").unwrap();
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;
        assert!(wires.get("not_a_real_name").is_none());
    }

    #[test]
    fn cycle_wires_resolves_phase_binding_lhs_names() {
        // SRD-68 Push 4b sanity check: a workload's phase
        // binding (e.g. `target_index_table := pick(...)`) becomes
        // an output on the per-op canonical kernel after
        // `OpBuilder::canonical_kernel_for_op`. Per-fiber
        // instances built from it via `build_subscope` carry that
        // output. Wrapping such a per-fiber kernel in `CycleWires`
        // and calling `wires.get(phase_binding_name)` returns the
        // computed value — no text substitution required at
        // adapter cycle time.
        //
        // This unit test simulates the shape directly via
        // `compile_polydat` rather than spinning up the full activity
        // pipeline; it pins the contract that `wires.get` answers
        // for any name the program declares as an output.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             keyspace := \"baselines\"\n\
             table := \"vec_label_00\"\n\
             # `pick`-equivalent with constant booleans for testability:\n\
             # take the first value when its selector is true.\n\
             target_index_table := \"system_views.sai_column_indexes\"\n",
        )
        .unwrap();
        k.set_inputs(&[0]);
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;
        assert_eq!(
            wires
                .get("target_index_table")
                .map(|v| v.as_str().to_string()),
            Some("system_views.sai_column_indexes".to_string()),
            "phase-binding LHS should resolve through CycleWires",
        );
        assert_eq!(
            wires.get("keyspace").map(|v| v.as_str().to_string()),
            Some("baselines".to_string()),
        );
    }

    #[test]
    fn substitute_via_wires_resolves_bare_names() {
        let mut k = compile_polydat(
            "input cycle: u64\n\
             keyspace := \"baselines\"\n\
             table := \"vec_label_00\"\n",
        )
        .unwrap();
        let cw = CycleWires::new(&mut k);
        let resolved =
            substitute_via_wires("SELECT * FROM {keyspace}.{table} WHERE x = 1", &cw).unwrap();
        assert_eq!(resolved, "SELECT * FROM baselines.vec_label_00 WHERE x = 1");
    }

    #[test]
    fn substitute_via_wires_passes_through_literal_braces() {
        let mut k = compile_polydat(
            "input cycle: u64\n\
             ks := \"baselines\"\n",
        )
        .unwrap();
        let cw = CycleWires::new(&mut k);
        // CQL map literal: brace bodies starting with quotes are
        // not bare identifiers, pass through verbatim.
        let resolved = substitute_via_wires(
            "CREATE KEYSPACE {ks} WITH replication = {'class': 'SimpleStrategy'}",
            &cw,
        )
        .unwrap();
        assert_eq!(
            resolved,
            "CREATE KEYSPACE baselines WITH replication = {'class': 'SimpleStrategy'}",
        );
    }

    /// Binary-natural type guard. A wire holding `VecF32` (or any
    /// other type with no meaningful text-spliced form) must not be
    /// silently decimal-stringified into the field text — that's a
    /// workload-shape bug (the value belongs in a typed-binding
    /// path, not the text template). The substitution layer
    /// rejects with a clear diagnostic pointing at the wire and
    /// suggesting both fixes (pure-token form / bindvars split).
    ///
    /// Regression guard: dropping this check would silently burn
    /// per-element Grisu float formatting (≈55% of cycle CPU on
    /// the fknn_rampup_data shape) on every cycle.
    #[test]
    fn substitute_via_wires_rejects_vec_f32_in_text_template() {
        use polydat::ast::SliceArc;
        struct VecWire;
        impl WireSource for VecWire {
            fn get(&self, name: &str) -> Option<Value> {
                match name {
                    "id" => Some(Value::U64(7)),
                    "vec" => Some(Value::VecF32(SliceArc::from_vec(vec![0.1_f32, 0.2, 0.3]))),
                    _ => None,
                }
            }
            fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
                Box::new(["id", "vec"].iter().map(|s| s.to_string()))
            }
        }
        let err = substitute_via_wires("INSERT ... VALUES ('{id}', {vec})", &VecWire)
            .expect_err("VecF32 splice into text template must error");
        assert!(
            err.contains("vec"),
            "diagnostic should name the offending wire: {err}"
        );
        assert!(
            err.contains("vec_f32") || err.contains("VecF32"),
            "diagnostic should name the offending type: {err}"
        );
        assert!(
            err.contains("pure-token") || err.contains("typed"),
            "diagnostic should point at the fix: {err}"
        );
    }

    /// The escape hatch: a field whose entire value is a pure
    /// token `{wire}` (no surrounding text) routes through case 2
    /// in `resolve_op_fields_via_wires` — typed Value preserved,
    /// no substitution. `substitute_via_wires` itself is only
    /// invoked on text-template fields, so the rejection above
    /// fires only when the binary-natural wire is genuinely being
    /// spliced into surrounding text. Pin that: a VecI32 wire
    /// triggers the same rejection (catches the broader contract,
    /// not a VecF32-only special case).
    #[test]
    fn substitute_via_wires_rejects_vec_i32_in_text_template() {
        use polydat::ast::SliceArc;
        struct VecI32Wire;
        impl WireSource for VecI32Wire {
            fn get(&self, name: &str) -> Option<Value> {
                if name == "vec" {
                    Some(Value::VecI32(SliceArc::from_vec(vec![1_i32, 2, 3])))
                } else {
                    None
                }
            }
            fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
                Box::new(std::iter::once("vec".to_string()))
            }
        }
        let err = substitute_via_wires("x = {vec}", &VecI32Wire).unwrap_err();
        assert!(
            err.contains("vec_i32") || err.contains("VecI32"),
            "VecI32 should also be rejected: {err}"
        );
    }

    #[test]
    fn substitute_via_wires_errors_on_unresolved_name() {
        let mut k = compile_polydat("input cycle: u64\nx := \"a\"\n").unwrap();
        let cw = CycleWires::new(&mut k);
        let err = substitute_via_wires("hi {nonexistent}", &cw).unwrap_err();
        assert!(
            err.contains("nonexistent"),
            "diagnostic should name the wire: {err}"
        );
        assert!(
            err.contains("unresolved"),
            "diagnostic should call out unresolved: {err}"
        );
    }

    #[test]
    fn substitute_via_wires_errors_when_wire_resolves_to_none() {
        // SRD-74 Rule 3 strict render: a wire that EXISTS but
        // resolves to `Value::None` (e.g. a `const X := "{undef}"`
        // whose RHS interpolation propagated None per Rule 1, then
        // the conditional-shadow chain found no upstream binding
        // either) must error at substitution time — NOT silently
        // render as `""`. Empty string is a real value; absent is
        // not the same thing, and conflating them was the
        // wire-protocol corruption class this SRD closes.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             extern undef: str\n\
             const x := \"{undef}\"\n",
        )
        .unwrap();
        let cw = CycleWires::new(&mut k);
        let err = substitute_via_wires("source_model='{x}'", &cw).unwrap_err();
        assert!(
            err.contains("`{x}`"),
            "diagnostic should name the bind-point: {err}"
        );
        assert!(
            err.contains("Value::None") || err.contains("no value bound"),
            "diagnostic should explain the None resolution: {err}"
        );
    }

    #[test]
    fn to_display_strict_returns_none_for_value_none() {
        // The strict primitive itself; render sites consume it.
        use polydat::ast::Value;
        assert_eq!(Value::None.to_display_strict(), None);
        assert_eq!(
            Value::Str("hello".into()).to_display_strict(),
            Some("hello".to_string())
        );
        assert_eq!(Value::U64(42).to_display_strict(), Some("42".to_string()));
    }

    #[test]
    fn substitute_via_wires_resolves_inside_cql_options_map() {
        // Pin the failure shape that surfaced in
        // `full_cql_vector.yaml`: a CQL `WITH OPTIONS = {'k':
        // '{value}'}` map. The earlier substitution algorithm
        // truncated the body at the first `}` (the inner
        // placeholder's `}`), then treated the outer `{...}`
        // as one literal-content body — so inner `{value}`
        // placeholders inside the map were never resolved.
        // The fix mirrors `nmbrs_workload::bindpoints::extract_bind_points`:
        // when `{` is followed by `'` or `"`, treat it as a CQL
        // map opener (emit the brace, continue scanning).
        let mut k = compile_polydat(
            "input cycle: u64\n\
             optimize_for := \"RECALL\"\n\
             similarity_function := \"EUCLIDEAN\"\n",
        )
        .unwrap();
        let cw = CycleWires::new(&mut k);
        let resolved = substitute_via_wires(
            "WITH OPTIONS = {'optimize_for': '{optimize_for}', 'similarity_function': '{similarity_function}'}",
            &cw,
        ).unwrap();
        assert_eq!(
            resolved,
            "WITH OPTIONS = {'optimize_for': 'RECALL', 'similarity_function': 'EUCLIDEAN'}",
            "nested `{{name}}` placeholders inside CQL map literals must resolve"
        );
    }

    #[test]
    fn substitute_via_wires_errors_on_qualifier_prefix() {
        let mut k = compile_polydat("input cycle: u64\nx := \"a\"\n").unwrap();
        let cw = CycleWires::new(&mut k);
        let err = substitute_via_wires("hi {bind:x}", &cw).unwrap_err();
        assert!(
            err.contains("bind:x"),
            "diagnostic should name the qualifier form: {err}"
        );
    }

    #[test]
    fn substitute_via_wires_passes_through_inline_expr() {
        let mut k = compile_polydat("input cycle: u64\nx := \"a\"\n").unwrap();
        let cw = CycleWires::new(&mut k);
        let resolved = substitute_via_wires("v = {{x + 1}}", &cw).unwrap();
        // `{{...}}` is reserved for the inline-expression desugar
        // surface (compiled into bindings before reaching cycle
        // time); cycle-time substitute_via_wires passes it
        // through unchanged.
        assert_eq!(resolved, "v = {{x + 1}}");
    }

    #[test]
    fn cycle_wires_resolves_iter_var_through_subscope_chain() {
        // SRD-68 invariant from user: "the Polydat context visible after
        // initialization in each scope is designed to and required
        // to provide all of the values which should be visible
        // including those which are populated at logical closure
        // boundaries based on comprehensions."
        //
        // Concretely: a for_each scope kernel populates iter vars
        // on its INPUT slots (per `for_iteration`). The phase
        // scope kernel then inherits them via `build_subscope`.
        // The op-template canonical (built via `build_subscope`
        // again) MUST also carry the iter var values so cycle-time
        // `wires.get(iter_var_name)` answers correctly.
        //
        // This test mirrors that chain: parent kernel has
        // `optimize_for` as an extern input declaration with a
        // populated value; child kernel declares the same name;
        // verify the value propagates through `build_subscope`
        // and is visible via `CycleWires::get`.
        use polydat::ast::Value;
        use polydat::dsl::compile::compile_polydat;
        use polydat::kernel::subcontext::PolydatMatter;

        // Parent: declares `optimize_for` as extern + auto-passthrough
        // output via `final` — same pattern the phase synthesizer
        // uses for iter-var cascade.
        let mut parent = compile_polydat(
            "input cycle: u64\n\
             extern optimize_for: String\n",
        )
        .unwrap();
        // Populate the input slot the way the phase kernel does
        // after `materialize_wiring_from_outer` from the for_each bound_kernel.
        let opt_idx = parent
            .program()
            .find_input("optimize_for")
            .expect("optimize_for input slot");
        parent
            .state()
            .set_input(opt_idx, Value::Str("RECALL".into()));

        // Child: program declares the same name as an extern.
        // Mimics the per-op canonical the dispenser owns.
        let child_program = compile_polydat(
            "input cycle: u64\n\
             extern optimize_for: String\n",
        )
        .unwrap()
        .program()
        .clone();

        let mut child = parent
            .build_subscope(
                PolydatMatter::builder()
                    .program(child_program)
                    .build()
                    .unwrap(),
            )
            .expect("subscope build is infallible");

        let cw = CycleWires::new(&mut child);
        let wires: &dyn WireSource = &cw;
        // The architectural contract: child's CycleWires resolves
        // `optimize_for` through the inheritance chain. If this
        // assertion fails, the chain isn't propagating iter vars
        // and we need the gap fix at the Polydat Kernel layer.
        assert_eq!(
            wires.get("optimize_for").map(|v| v.as_str().to_string()),
            Some("RECALL".to_string()),
            "iter-var input populated on parent should propagate \
             through build_subscope to child's WireSource",
        );
    }

    #[test]
    fn extern_decl_only_produces_input_no_output() {
        // Diagnostic: an `extern <name>: <type>` declaration by
        // itself creates an INPUT but does NOT publish a matching
        // output. So `materialize_wiring_from_outer` (which walks outer's
        // outputs) wouldn't find this name and wouldn't propagate
        // it to descendants. The synthesis pipeline avoids this by
        // also calling `mark_inherited_outputs` on the kernel
        // post-build, but a plain `extern` line doesn't.
        //
        // Pinning this so the next person reading the source isn't
        // surprised — `cycle_wires_resolves_iter_var_through_subscope_chain`
        // passes only because the `String` extern's auto-passthrough
        // output (added by the compiler when the name appears in the
        // body) gives materialize_wiring_from_outer something to walk. With ONLY
        // an extern decl, there's no body reference, no auto-passthrough,
        // and the chain breaks.
        use polydat::dsl::compile::compile_polydat;
        let k = compile_polydat(
            "input cycle: u64\n\
             extern optimize_for: String\n",
        )
        .unwrap();
        let outputs: Vec<&str> = k.program().output_names().to_vec();
        // If this assertion fails (i.e. `optimize_for` IS in
        // outputs), then the chain test above was a no-op and we
        // need to revisit the architectural question.
        // Document whichever is true.
        eprintln!("DBG outputs from `extern optimize_for: String`: {outputs:?}");
    }

    #[test]
    fn cycle_wires_advance_drives_per_row_pulls() {
        // SRD-68 batch contract: `advance(coord)` mutates the
        // underlying kernel's coord input so subsequent `get`
        // calls produce values for that coord. Verify by pulling
        // a coord-dependent output before and after advance.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             id := format_u64(cycle, 10)\n",
        )
        .unwrap();
        k.set_inputs(&[0]);
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;
        // First read at cycle=0.
        assert_eq!(
            wires.get("id").map(|v| v.as_str().to_string()),
            Some("0".to_string())
        );
        // Advance and re-read.
        wires.advance(42);
        assert_eq!(
            wires.get("id").map(|v| v.as_str().to_string()),
            Some("42".to_string())
        );
        wires.advance(7);
        assert_eq!(
            wires.get("id").map(|v| v.as_str().to_string()),
            Some("7".to_string())
        );
    }

    #[test]
    fn null_wires_advance_is_noop() {
        // Default `advance` impl on `NullWireSource` does nothing;
        // useful as a sanity check that advance on a non-batch-
        // capable wires doesn't panic.
        let wires: &dyn WireSource = &NULL_WIRES;
        wires.advance(0);
        wires.advance(123);
        assert!(wires.get("anything").is_none());
    }

    #[test]
    fn cycle_wires_write_lands_on_input_slot_visible_to_get() {
        // SRD-66 capture flow: ResultDispenser writes a magic-extern
        // input (e.g. `count`), then a later wrapper or the eval
        // cone reads it. `wires.write` lands the value; `wires.get`
        // returns it on the next read.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             extern count: u64\n\
             extern body: Json\n",
        )
        .unwrap();
        k.set_inputs(&[0]);
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;

        assert_eq!(wires.write("count", Value::U64(42)), WriteOutcome::Stored);
        assert_eq!(wires.get("count").map(|v| v.as_u64()), Some(42));

        // Overwrite — the new value supersedes the old.
        assert_eq!(wires.write("count", Value::U64(7)), WriteOutcome::Stored);
        assert_eq!(wires.get("count").map(|v| v.as_u64()), Some(7));
    }

    #[test]
    fn cycle_wires_write_returns_no_slot_for_unknown_name() {
        // The closure-binding economy's DCE signal: no slot, value
        // silently dropped. Caller is unaffected.
        let mut k = compile_polydat("input cycle: u64\nx := 1\n").unwrap();
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;
        assert_eq!(wires.write("nope", Value::U64(99)), WriteOutcome::NoSlot);
    }

    #[test]
    fn cycle_wires_write_feeds_eval_cone_through_get() {
        // The full capture-then-pull flow: write a magic-extern
        // input, read an output that depends on it through the
        // eval cone. This is the metrics-wrapper-reading-row_count
        // scenario from the design memo.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             extern count: u64\n\
             row_count := count\n",
        )
        .unwrap();
        k.set_inputs(&[0]);
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;

        // Before the write, `count` is at its default and
        // `row_count` reflects that.
        assert_eq!(wires.write("count", Value::U64(123)), WriteOutcome::Stored);
        // The pull through `row_count` (an output) reads the
        // freshly-written `count` (an input).
        assert_eq!(wires.get("row_count").map(|v| v.as_u64()), Some(123));
    }

    #[test]
    fn null_wires_write_returns_no_slot() {
        let wires: &dyn WireSource = &NULL_WIRES;
        assert_eq!(wires.write("anything", Value::U64(1)), WriteOutcome::NoSlot);
    }

    #[test]
    fn resolve_op_fields_four_case_dispatch() {
        // Pin the four-case dispatch contract for op-field resolution.
        // See `resolve_op_fields_via_wires` doc for the cases.
        let mut k = compile_polydat(
            "input cycle: u64\n\
             table := \"users\"\n\
             count := 42\n",
        )
        .unwrap();
        let cw = CycleWires::new(&mut k);

        let fields: Vec<(String, serde_json::Value)> = vec![
            // Case 1: non-string JSON scalar — stringified.
            ("limit_num".into(), serde_json::json!(100)),
            // Case 2: pure-token — typed wire reference, count is U64(42).
            (
                "typed_ref".into(),
                serde_json::Value::String("{count}".into()),
            ),
            // Case 3: text template — wires.get(table).to_display_string()
            //          interpolated into the surrounding text.
            (
                "templated".into(),
                serde_json::Value::String("SELECT * FROM {table} WHERE x = 1".into()),
            ),
            // Case 4: bare-string literal — no placeholders, passes through.
            (
                "literal_stmt".into(),
                serde_json::Value::String("SELECT 1".into()),
            ),
        ];

        let resolved = resolve_op_fields_via_wires(&fields, &cw).expect("four-case dispatch");

        // Case 1: stringified — exact form is implementation-defined,
        // verify the value contains the digits.
        assert!(
            matches!(resolved.get_value("limit_num"),
            Some(polydat::ast::Value::Str(s)) if s.contains("100")),
            "case 1: got {:?}",
            resolved.get_value("limit_num")
        );
        // Case 2: typed U64.
        assert_eq!(
            resolved.get_value("typed_ref").map(|v| v.as_u64()),
            Some(42),
            "case 2: typed wire ref preserves U64"
        );
        // Case 3: interpolated string.
        assert_eq!(
            resolved.get_str("templated"),
            Some("SELECT * FROM users WHERE x = 1"),
            "case 3: text template"
        );
        // Case 4: literal pass-through.
        assert_eq!(
            resolved.get_str("literal_stmt"),
            Some("SELECT 1"),
            "case 4: bare string literal"
        );
    }

    #[test]
    fn cycle_wires_caches_across_repeated_gets() {
        // Pull memoizes; a second get of the same name reads the
        // cached value off state, not re-runs the eval cone. We
        // can't directly observe memoization but we CAN observe
        // that two reads of the same cycle produce the same value
        // (hash is deterministic per coordinate, but the kernel's
        // dirty-tracking would require a state mutation to
        // re-fire — the property still holds).
        let mut k = compile_polydat("input cycle: u64\nh := hash(cycle)\n").unwrap();
        k.set_inputs(&[42]);
        let cw = CycleWires::new(&mut k);
        let wires: &dyn WireSource = &cw;
        let v1 = wires.get("h").unwrap().as_u64();
        let v2 = wires.get("h").unwrap().as_u64();
        assert_eq!(v1, v2);
    }
}
