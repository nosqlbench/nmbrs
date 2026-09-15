// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Typed binding scope model for Polydat Kernel compilation.
//!
//! A `BindingScope` is the structured intermediate representation
//! that replaces raw string manipulation for scope composition.
//! Every binding carries its provenance (`BindingOrigin`), its
//! modifier, and its definition text. Scope rules are checked
//! against this typed structure, and a single deduplicated GK
//! source string is emitted at the end.

use std::collections::{HashMap, HashSet};

use crate::scope_synth::{
    collect_leaf_placeholders, value_to_param_string, workload_param_type_name,
};
use nmbrs_workload::model::{BindingsDef, ParsedOp};
use polydat::kernel::interp::collect_string_interp_refs;

/// Where a binding was declared — its provenance in the scope chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingOrigin {
    /// Declared at a YAML level outside the op (today only block
    /// sugar, since SRD-13f Push D retired the workload/phase
    /// parser-merge into ops). Reaches the op via
    /// `inline_block_sugar_into_op` during YAML parsing.
    Inherited,
    /// Declared at phase level (the phase has its own `bindings:` block).
    Phase,
    /// Declared at op level (op-specific augmentation).
    Op(String),
    /// Injected as a `for_each` iteration variable.
    IterationVar,
    /// Generated as an `extern` from the outer scope manifest.
    AutoExtern,
    /// Injected from workload param expansion.
    ParamExpansion,
    /// Generated from inline expression extraction (`{{expr}}`).
    InlineExpr,
}

impl std::fmt::Display for BindingOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inherited => write!(f, "inherited"),
            Self::Phase => write!(f, "phase"),
            Self::Op(name) => write!(f, "op '{name}'"),
            Self::IterationVar => write!(f, "iteration variable"),
            Self::AutoExtern => write!(f, "auto-extern"),
            Self::ParamExpansion => write!(f, "param expansion"),
            Self::InlineExpr => write!(f, "inline expression"),
        }
    }
}

/// The modifier on a binding declaration. Mirrors the
/// `BindingModifier` flag set in polydat' Polydat AST plus
/// the binding-kind keywords (`init`, `cursor`) that
/// nmbrs-runtime's text-level scope assembly cares about.
///
/// Wire-coloring modifiers (`final`, `shared`, `volatile`)
/// could in principle combine, but the scope assembly path
/// historically only needs to know "is this final?" /
/// "is this shared?" for shadow checks; combinations get
/// reduced here to the most-distinctive single tag. The full
/// flag set is preserved in the eventual Polydat AST when the
/// scope-emitted source compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeModifier {
    None,
    Init,
    Shared,
    Final,
    Volatile,
    Cursor,
}

/// A single binding declaration with provenance.
#[derive(Debug, Clone)]
pub struct ScopedBinding {
    /// The binding name (LHS of `:=`).
    pub name: String,
    /// The full declaration line as Polydat source text.
    /// For regular bindings: `"name := expr"`
    /// For init: `"const name := \"value\""`
    /// For externs: `"extern name: Type"`
    pub line: String,
    /// Where this binding came from.
    pub origin: BindingOrigin,
    /// Modifier on the declaration.
    pub modifier: ScopeModifier,
}

/// A typed extern declaration.
#[derive(Debug, Clone)]
pub struct ExternDecl {
    pub name: String,
    pub type_name: String,
}

/// Typed scope for a phase's Polydat Kernel compilation.
///
/// Built by the executor from structured inputs, validated for
/// scope rules, then emitted as a single Polydat source string.
pub struct BindingScope {
    /// The coordinate declaration (e.g., `"input cycle: u64"`).
    coordinates: Option<String>,
    /// All bindings in insertion order.
    bindings: Vec<ScopedBinding>,
    /// Extern declarations.
    externs: Vec<ExternDecl>,
    /// Names referenced by op templates (for DCE).
    required_outputs: Vec<String>,
    /// Extra required outputs from config expressions.
    config_refs: Vec<String>,
}

impl Default for BindingScope {
    fn default() -> Self {
        Self::new()
    }
}

impl BindingScope {
    /// Create an empty scope.
    pub fn new() -> Self {
        Self {
            coordinates: None,
            bindings: Vec::new(),
            externs: Vec::new(),
            required_outputs: Vec::new(),
            config_refs: Vec::new(),
        }
    }

    /// Ingest bindings from a `BindingsDef::PolydatSource`, classifying each
    /// line by the given origin. Extracts coordinates and handles all
    /// Polydat declaration forms (init, shared, final, cursor, extern, plain).
    ///
    /// A "line" here is a *logical* line: physical newlines inside
    /// unbalanced `()`/`[]`/`{}` or inside a string literal are
    /// absorbed into the current binding. This is what lets multi-line
    /// expressions like
    ///
    /// ```polydat
    /// rate_adjust := control_set("rate",
    ///                            to_f64(control_u64("rate")) * 1.05)
    /// ```
    ///
    /// survive the later split-on-`\n` in [`Self::emit`]: we rejoin
    /// them onto one physical line so the downstream parser sees a
    /// complete expression.
    pub fn ingest_polydat_source(&mut self, source: &str, origin: BindingOrigin) {
        for line in logical_lines(source) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
                continue;
            }

            // `input` declarations: `input <name>[: <type>]` (bare) or
            // `input (<name>[: <type>], ...)` (tuple). Stored verbatim
            // on the scope's `coordinates` slot — the synthesizer
            // re-emits this line into the per-scope Polydat source so the
            // compiler can pick up the declared inputs.
            if trimmed.starts_with("input ") {
                self.coordinates = Some(trimmed.to_string());
                continue;
            }

            // Cursor declarations use `=` not `:=`:
            //   cursor row = range(0, vector_count("example"))
            //   const prebuffer := dataset_prebuffer("example")
            // These are Polydat statements that the compiler handles directly.
            // Pass them through as bindings so they survive emission.
            if (trimmed.starts_with("cursor ") || trimmed.starts_with("init "))
                && let Some(eq_pos) = trimmed.find('=')
            {
                // Check it's `=` not `:=`
                let before_eq = &trimmed[..eq_pos];
                if !before_eq.ends_with(':') {
                    // cursor/init with bare `=` — pass through as-is
                    let lhs = before_eq.trim();
                    let (modifier, name) = parse_modifier_and_name(lhs);
                    self.bindings.push(ScopedBinding {
                        name: name.to_string(),
                        line: trimmed.to_string(),
                        origin: origin.clone(),
                        modifier,
                    });
                    continue;
                }
            }

            if let Some(pos) = trimmed.find(":=") {
                let lhs = trimmed[..pos].trim();

                // Extern declarations
                if lhs.starts_with("extern") {
                    // Already handled via add_extern; skip inline externs
                    // from inherited sources
                    continue;
                }

                // Determine modifier and extract bare name
                let (modifier, name) = parse_modifier_and_name(lhs);

                self.bindings.push(ScopedBinding {
                    name: name.to_string(),
                    line: trimmed.to_string(),
                    origin: origin.clone(),
                    modifier,
                });
            } else if trimmed.starts_with("extern ") {
                // `extern name: Type` (no `:=`)
                if let Some(colon_pos) = trimmed.find(':') {
                    let name = trimmed["extern ".len()..colon_pos].trim();
                    let type_name = trimmed[colon_pos + 1..].trim();
                    self.externs.push(ExternDecl {
                        name: name.to_string(),
                        type_name: type_name.to_string(),
                    });
                }
            }
            // Lines that don't match any pattern are silently skipped.
            // Comments and blank lines are already filtered above.
        }
    }

    /// Add an iteration variable from `for_each`.
    ///
    /// Iteration variables are declared as `extern` ports rather
    /// than init-time bindings (SRD 18b §"Iteration variables as
    /// scope outputs"). The runtime sets the extern's value
    /// before the leaf kernel executes, so we no longer
    /// text-substitute literal values into the Polydat source. The
    /// type is inferred from the current iteration's value:
    /// numeric strings get `u64`/`f64`, anything else is `String`.
    pub fn add_iteration_var(&mut self, name: &str, value: &str) {
        let type_name = if value.parse::<u64>().is_ok() {
            "u64"
        } else if value.parse::<f64>().is_ok() {
            "f64"
        } else {
            "String"
        };
        self.externs.push(ExternDecl {
            name: name.to_string(),
            type_name: type_name.to_string(),
        });
    }

    /// Add an auto-extern declaration from the outer scope manifest.
    pub fn add_extern(&mut self, name: &str, type_name: &str) {
        self.externs.push(ExternDecl {
            name: name.to_string(),
            type_name: type_name.to_string(),
        });
    }

    /// Add a workload param binding as a `final` (compile-time
    /// constant) binding so the compiler folds the literal value
    /// and the assembler treats references as const args rather
    /// than wire inputs. The `const` modifier matches the M3.6
    /// contract: workload params are immutable for the run, so
    /// downstream nodes consume them as constants.
    ///
    /// String values are emitted as quoted Polydat string literals
    /// with embedded `"` and `\` escaped, so JSON-shaped param
    /// values (`{"a": 1}`, `{'class': 'SimpleStrategy'}`,
    /// arbitrary nested quotes) round-trip through GK
    /// compilation unchanged. Numeric and boolean values are
    /// emitted as bare literals.
    pub fn add_param_binding(&mut self, name: &str, value: &str) {
        // Param-binding emission used by scope synth when
        // cascading workload-root values into descendant
        // scopes. Values arrive already-typed (raw workload-
        // param text OR `value_to_param_string` output from a
        // parent kernel constant). Workload-root semantics
        // apply: bare strings lower to polydat string literals
        // (legacy), NOT references. The `set:` block parser
        // in nmbrs-workload uses a DIFFERENT classifier that
        // does treat bare identifiers as references — that's
        // the entry point where the operator IS in a scope
        // context with visible wires to reference.
        let trimmed = value.trim();
        // A bare numeric/bool literal, or an already-quoted polydat
        // string / array literal, passes through verbatim; anything else
        // is wrapped as a quoted polydat string. (Both pass-through cases
        // yield the same text, so they share one arm.)
        let literal = if trimmed.parse::<u64>().is_ok()
            || trimmed.parse::<f64>().is_ok()
            || trimmed == "true"
            || trimmed == "false"
            || is_polydat_quoted_string(trimmed)
            || is_polydat_array_literal(trimmed)
        {
            trimmed.to_string()
        } else {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        };
        self.bindings.push(ScopedBinding {
            name: name.to_string(),
            line: format!("const {name} := {literal}"),
            origin: BindingOrigin::ParamExpansion,
            modifier: ScopeModifier::Final,
        });
    }

    /// Add an inline expression binding.
    pub fn add_inline_expr(&mut self, name: &str, expr: &str) {
        self.bindings.push(ScopedBinding {
            name: name.to_string(),
            line: format!("{name} := {expr}"),
            origin: BindingOrigin::InlineExpr,
            modifier: ScopeModifier::None,
        });
    }

    /// Register a name referenced by an op template (for DCE).
    pub fn add_required_output(&mut self, name: &str) {
        if !self.required_outputs.contains(&name.to_string()) {
            self.required_outputs.push(name.to_string());
        }
    }

    /// Register a config expression reference (for DCE).
    pub fn add_config_ref(&mut self, name: &str) {
        if !self.config_refs.contains(&name.to_string()) {
            self.config_refs.push(name.to_string());
        }
    }

    /// All names defined in this scope (all origins).
    pub fn defined_names(&self) -> HashSet<String> {
        self.bindings.iter().map(|b| b.name.clone()).collect()
    }

    /// All extern names in this scope.
    pub fn extern_names(&self) -> HashSet<String> {
        self.externs.iter().map(|e| e.name.clone()).collect()
    }

    /// The combined required outputs (template refs + config refs).
    pub fn required_outputs(&self) -> Vec<String> {
        let mut all = self.required_outputs.clone();
        for name in &self.config_refs {
            if !all.contains(name) {
                all.push(name.clone());
            }
        }
        all
    }

    /// Validate scope rules. Returns `Ok(())` if valid, or a
    /// descriptive error explaining the violation and its provenance.
    pub fn validate(&self) -> Result<(), String> {
        // Build a map of name → first binding for each origin tier.
        // The tier order determines precedence: earlier tiers own the name.
        let mut owned: HashMap<String, &ScopedBinding> = HashMap::new();

        for binding in &self.bindings {
            if let Some(prior) = owned.get(&binding.name) {
                // Same name appears twice. Check if this is allowed.
                match (&prior.origin, &binding.origin) {
                    // Inherited + Inherited: duplicate from multiple ops
                    // sharing the same workload bindings. Allowed if
                    // definitions are identical.
                    (BindingOrigin::Inherited, BindingOrigin::Inherited) => {
                        if prior.line != binding.line {
                            return Err(format!(
                                "binding '{}' has conflicting inherited definitions:\n  \
                                 first:  {}\n  second: {}",
                                binding.name, prior.line, binding.line
                            ));
                        }
                        // Duplicate with same definition — will be deduplicated at emit
                    }

                    // Phase + Phase: same check as inherited
                    (BindingOrigin::Phase, BindingOrigin::Phase) => {
                        if prior.line != binding.line {
                            return Err(format!(
                                "binding '{}' has conflicting phase-level definitions:\n  \
                                 first:  {}\n  second: {}",
                                binding.name, prior.line, binding.line
                            ));
                        }
                    }

                    // Op overriding Inherited/Phase/IterationVar with a
                    // DIFFERENT definition: real shadow, error.
                    (
                        BindingOrigin::Inherited
                        | BindingOrigin::Phase
                        | BindingOrigin::IterationVar,
                        BindingOrigin::Op(op_name),
                    ) => {
                        if prior.line != binding.line {
                            return Err(format!(
                                "op '{}' binding '{}' shadows a name from {} origin \
                                 with a different definition.\n  \
                                 scope: {}\n  op:    {}\n\
                                 Ops augment the scope DAG but cannot override it. \
                                 Use a separate phase for different bindings.",
                                op_name, binding.name, prior.origin, prior.line, binding.line
                            ));
                        }
                        // Same definition from inheritance — dedup at emit
                    }

                    // Op redefining an op binding from a DIFFERENT op
                    (BindingOrigin::Op(prior_op), BindingOrigin::Op(this_op)) => {
                        if prior_op != this_op {
                            return Err(format!(
                                "op '{}' binding '{}' is already defined by op '{}'. \
                                 Each ride-along binding name must be unique across \
                                 all ops in the scope.",
                                this_op, binding.name, prior_op
                            ));
                        }
                        // Same op, same name — shouldn't happen, but tolerate if same def
                    }

                    // IterationVar replacing Inherited/Phase: iteration
                    // variables are injected before inherited bindings
                    // in the emission order, so Polydat sees them first.
                    // This is intentional — for_each vars override params.
                    (
                        BindingOrigin::IterationVar,
                        BindingOrigin::Inherited | BindingOrigin::Phase,
                    )
                    | (
                        BindingOrigin::Inherited | BindingOrigin::Phase,
                        BindingOrigin::IterationVar,
                    ) => {
                        // Allowed: iteration vars intentionally override inherited names
                    }

                    // ParamExpansion: these are only added when the name
                    // is NOT already defined. The caller checks. But if
                    // somehow a duplicate appears, same-def is fine.
                    (_, BindingOrigin::ParamExpansion) | (BindingOrigin::ParamExpansion, _) => {
                        // Param expansion is additive; caller skips existing names
                    }

                    // InlineExpr: synthetic names (__expr_N), shouldn't collide
                    (_, BindingOrigin::InlineExpr) | (BindingOrigin::InlineExpr, _) => {
                        // Synthetic names from inline expressions
                    }

                    // AutoExtern: these go to the extern list, not bindings.
                    // Shouldn't appear here, but tolerate.
                    (_, BindingOrigin::AutoExtern) | (BindingOrigin::AutoExtern, _) => {}

                    // Inherited + Op(same def): already covered above.
                    // Any other combination: flag it.
                    _ => {
                        if prior.line != binding.line {
                            return Err(format!(
                                "binding '{}' conflicts: {} origin ({}) vs {} origin ({})",
                                binding.name,
                                prior.origin,
                                prior.line,
                                binding.origin,
                                binding.line
                            ));
                        }
                    }
                }
            } else {
                owned.insert(binding.name.clone(), binding);
            }
        }

        // Check final shadowing: no binding can redefine a name that
        // appears as Final in the extern list or prior bindings.
        let final_names: HashSet<String> = self
            .bindings
            .iter()
            .filter(|b| b.modifier == ScopeModifier::Final)
            .map(|b| b.name.clone())
            .collect();

        for binding in &self.bindings {
            if final_names.contains(&binding.name)
                && binding.modifier != ScopeModifier::Final
                && binding.origin != BindingOrigin::Inherited
            {
                return Err(format!(
                    "cannot shadow 'final' binding '{}' from outer scope",
                    binding.name
                ));
            }
        }

        Ok(())
    }

    /// Emit the validated scope as a single Polydat source string.
    ///
    /// Entries are ordered:
    /// 1. Coordinates declaration
    /// 2. Extern declarations
    /// 3. Init declarations (iteration variables)
    /// 4. Inherited/Phase bindings (deduplicated by name)
    /// 5. ParamExpansion bindings
    /// 6. InlineExpr bindings
    /// 7. Op-level bindings (new names only)
    ///
    /// Each name is emitted exactly once. The first occurrence wins;
    /// subsequent duplicates with the same definition are suppressed.
    pub fn emit(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut emitted_names: HashSet<String> = HashSet::new();

        // 1. Coordinates — emit only when an ingested source
        // declared them. The Polydat compiler auto-infers coordinates
        // from `cycle` references in non-strict mode, so a
        // workload-level scope built purely from injected
        // workload params (no op-supplied Polydat source, no `cycle`
        // references) compiles fine without a synthetic line.
        if let Some(ref coords) = self.coordinates {
            lines.push(coords.clone());
            // Populate `emitted_names` from the declared coordinate
            // names so the externs pass (and downstream binding
            // emission) doesn't re-introduce a same-named declaration
            // that would collide with the input slot. The coord line
            // is the canonical `input` declaration in either
            // surface form:
            //   input <name>[: <type>]               (bare)
            //   input (<name>[: <type>], ...)        (tuple)
            scan_input_decl_names(coords.trim(), &mut emitted_names);
        }

        // 2. Externs
        for ext in &self.externs {
            if !emitted_names.contains(&ext.name) {
                lines.push(format!("extern {}: {}", ext.name, ext.type_name));
                emitted_names.insert(ext.name.clone());
            }
        }

        // 3-7. Bindings in origin order
        let origin_order: &[fn(&BindingOrigin) -> bool] = &[
            |o| matches!(o, BindingOrigin::IterationVar),
            |o| matches!(o, BindingOrigin::Inherited),
            |o| matches!(o, BindingOrigin::Phase),
            |o| matches!(o, BindingOrigin::ParamExpansion),
            |o| matches!(o, BindingOrigin::InlineExpr),
            |o| matches!(o, BindingOrigin::Op(_)),
        ];

        for predicate in origin_order {
            for binding in &self.bindings {
                if predicate(&binding.origin) && !emitted_names.contains(&binding.name) {
                    lines.push(binding.line.clone());
                    emitted_names.insert(binding.name.clone());
                }
            }
        }

        lines.join("\n")
    }
}

/// Parse the modifier prefix and bare name from a Polydat LHS.
///
/// `"shared foo"` → `(Shared, "foo")`
/// `"init bar"` → `(Init, "bar")`
/// `"cursor baz"` → `(Cursor, "baz")`
/// `"final qux"` → `(Final, "qux")`
/// `"plain"` → `(None, "plain")`
/// Split `source` into logical lines, treating newlines inside
/// unbalanced `()`, `[]`, `{}` or string literals as continuations.
///
/// A logical line ends at the first physical newline that sits at
/// bracket-depth 0 and outside any string. Each returned String has
/// its interior newlines collapsed to single spaces so the downstream
/// Polydat parser sees one-expression-per-line, which is all it supports.
fn logical_lines(source: &str) -> Vec<String> {
    // Polydat grammar uses ONLY `"`-delimited string literals (see
    // `polydat/src/dsl/lexer.rs`). Apostrophes (`'`) carry
    // no special meaning at the lexical level — they appear
    // verbatim in comments ("the workload's bindings") and
    // inside double-quoted strings, never as a string delimiter
    // themselves. Treating `'` as a delimiter here would let a
    // stray apostrophe in a comment swallow the entire rest of
    // the source as a single unterminated "string" run, which
    // then collapses every subsequent binding into one logical
    // line that the per-line parser sees as a giant comment
    // and silently drops.
    //
    // Comments (`#`-to-EOL and `//`-to-EOL) are NOT skipped at
    // this level — physical newlines inside them terminate the
    // logical line uniformly, and the per-line `ingest_polydat_source`
    // pass skips any `#`/`//`-leading line via `trimmed.starts_with`.
    // What matters at this level is that bracket/string state
    // doesn't get confused by content inside comments.
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut in_line_comment = false;
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_line_comment {
            buf.push(ch);
            if ch == '\n' {
                in_line_comment = false;
                // Comments live at depth 0 by definition (they're
                // line-oriented); the newline always terminates
                // the logical line.
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            continue;
        }
        if in_str {
            buf.push(ch);
            if ch == '\\' {
                if let Some(nx) = chars.next() {
                    buf.push(nx);
                }
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                buf.push(ch);
            }
            '#' => {
                // `#`-to-EOL comment — stop tracking bracket
                // depth and string state inside it so a stray
                // apostrophe or `(` in the comment doesn't
                // imbalance later real-code parsing.
                in_line_comment = true;
                buf.push(ch);
            }
            '/' if matches!(chars.peek(), Some('/')) => {
                // `//`-to-EOL comment, same treatment as `#`.
                in_line_comment = true;
                buf.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                buf.push(ch);
            }
            ')' | ']' | '}' => {
                if depth > 0 {
                    depth -= 1;
                }
                buf.push(ch);
            }
            '\n' => {
                if depth > 0 {
                    buf.push(' ');
                } else {
                    out.push(std::mem::take(&mut buf));
                }
            }
            _ => buf.push(ch),
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Format a `Value` as a natural-form string suitable for
/// passing to [`BindingScope::add_param_binding`]. `add_param_binding`
/// recognises u64/f64-shaped strings, the literals `true` / `false`,
/// and quotes everything else.
///
/// Returns `None` for value types that can't be represented as a
/// Polydat source literal (`Bytes`, `Json`, `Ext`, `Handle`, vectors,
/// `None`). The synthesizer falls back to extern cascade in
/// those cases, since promoted-final inlining only works when
/// the value can round-trip through source.
/// Parse an `input` declaration line and insert each declared
/// slot name into `out`. Accepts both surface forms:
///
/// - `input cycle: u64`       — bare single (with or without type)
/// - `input (a: u64, b: f64)` — tuple form
///
/// The leading `input ` keyword may be present or absent — the
/// scanner tolerates a coord line stored without the keyword (older
/// emit paths) by falling back to treating the body as the slot
/// list. Empty / unparseable input is a no-op.
pub(crate) fn scan_input_decl_names(line: &str, out: &mut HashSet<String>) {
    let body = line.trim().strip_prefix("input ").unwrap_or(line.trim());
    let body = body.trim();
    if let Some(inner) = body.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        for part in inner.split(',') {
            let name = part.trim().split(':').next().unwrap_or("").trim();
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
        return;
    }
    let name = body.split(':').next().unwrap_or("").trim();
    if !name.is_empty() {
        out.insert(name.to_string());
    }
}

/// Render an `input` declaration line for one or more slot names,
/// matching the two surface forms produced by the parser:
///
/// - one name           → `input <name>: u64\n`
/// - multiple names     → `input (<a>: u64, <b>: u64, ...)\n`
///
/// All slots default to `u64` here — the runtime emits these for
/// propagated parent inputs, which are already u64-typed today.
/// A future port-type-aware pass can plumb the parent's declared
/// type through if/when non-u64 inputs become common.
fn format_input_decl_line(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [single] => format!("input {single}: u64\n"),
        many => {
            let typed: Vec<String> = many.iter().map(|n| format!("{n}: u64")).collect();
            format!("input ({})\n", typed.join(", "))
        }
    }
}

fn parse_modifier_and_name(lhs: &str) -> (ScopeModifier, &str) {
    // Strip every recognised modifier prefix before classifying;
    // multi-modifier forms like `volatile final` or `final shared`
    // only see a single ScopeModifier tag at the scope-assembly
    // level (the most-distinctive one wins), but the bare name
    // still gets extracted correctly. The eventual Polydat compile
    // sees the full source line with all keywords intact.
    let mut rest = lhs;
    let mut tag = ScopeModifier::None;
    loop {
        let prev = rest;
        if let Some(r) = rest.strip_prefix("shared ") {
            rest = r.trim();
            // `final` / `init` / `cursor` are more distinctive
            // than `shared` for shadow / kind checks; only set
            // tag if we don't already have a stronger one.
            if matches!(tag, ScopeModifier::None | ScopeModifier::Volatile) {
                tag = ScopeModifier::Shared;
            }
        } else if let Some(r) = rest.strip_prefix("const ") {
            // `const` is the current spelling of the Final tier
            // (init/final retired) — same tag as legacy `final`.
            rest = r.trim();
            tag = ScopeModifier::Final;
        } else if let Some(r) = rest.strip_prefix("final ") {
            rest = r.trim();
            tag = ScopeModifier::Final;
        } else if let Some(r) = rest.strip_prefix("volatile ") {
            rest = r.trim();
            if matches!(tag, ScopeModifier::None) {
                tag = ScopeModifier::Volatile;
            }
        } else if let Some(r) = rest.strip_prefix("init ") {
            rest = r.trim();
            tag = ScopeModifier::Init;
            break; // init is a kind-keyword; no modifiers come after it.
        } else if let Some(r) = rest.strip_prefix("cursor ") {
            rest = r.trim();
            tag = ScopeModifier::Cursor;
            break;
        }
        if rest == prev {
            break;
        }
    }
    (tag, rest)
}

// Build a `BindingScope` from phase ops and execution context.
//
// This is the main entry point that replaces the string mutation
// pipeline in the executor. It:
// 1. Classifies each op's bindings by origin (Inherited vs Op)
// 2. Adds iteration variables
// 3. Generates auto-externs from the outer manifest
// 4. Expands workload params
// 5. Extracts inline expressions
// 6. Collects required outputs from op templates
// =========================================================================
// For-each / comprehension scope kernel synthesis (M3.2)
// =========================================================================
//
// The for_each / for_combinations / for_each_union synthesizer
// lives in `polydat::iteration::comprehension::synthesis::synthesize_for_each_scope`.
// Callers in this crate go directly to that entry point; this
// module retains only the do-loop synthesizer below, since
// do_while / do_until aren't comprehensions.

/// Build the per-scope Polydat Kernel for a `do_while` / `do_until`
/// node (SRD 18b). Same composition contract as for_each
/// (every name visible at this scope resolves through standard
/// Polydat API on the synthesized kernel) — the difference is the
/// "scope output" is a `counter: u64` rather than tuple
/// iteration variables, and there's no value list to pre-eval.
///
/// Source shape:
///
/// ```text
///   extern <inherited_name>: <type>      # one per name
///                                        # referenced in
///                                        # `condition` text
///                                        # that exists in
///                                        # the parent
///                                        # manifest or
///                                        # workload params
///   final <inherited_param> := <literal> # workload-param
///                                        # injection (M3.3
///                                        # bridge until M3.6)
///   extern <counter>: u64                # only when counter
///                                        # is `Some`
/// ```
///
/// Per iteration the runtime sets `counter` (if present) via
/// `kernel.state().set_input` and evaluates the condition
/// expression against the kernel via `interpolate_via_kernel` +
/// `eval_const_expr`. Children inherit `counter` (and any
/// inherited names) through standard `materialize_wiring_from_outer`.
/// Synthesize the Polydat scope kernel for a phase that carries its own
/// `bindings:` block.
///
/// The phase scope owns this kernel as part of its closure lifetime
/// (per the Polydat builder/walk/instancing protocol): a phase whose YAML
/// declared `bindings: |` produces matter that the parent kernel's
/// `build_subscope` materializes into a layered kernel. Op-template
/// scopes that descend from this phase find these bindings as
/// outputs of their parent kernel and `extern` them through the
/// standard manifest cascade.
///
/// Phases without bindings AND without `for_each:` produce no install
/// spec — their scope-tree node carries an empty `cached_kernel` and
/// the parent walker resolves through to the nearest ancestor with a
/// kernel. The closure invariant ("every scope has a kernel
/// reference") still holds; the reference is just the parent's,
/// matter-gated as the Polydat APIs prescribe.
///
/// Source emitted (in order):
///
/// ```text
///   final <workload_param> := <literal>     # cascaded params
///   extern <ancestor_output>: <type>        # cascaded outputs/inputs
///   <phase_bindings_body>                   # phase-declared bindings
/// ```
///
/// The phase's bindings body is appended verbatim so the Polydat compiler
/// classifies them per the same rules as op-level bindings (init /
/// shared / final detection, type inference). Iteration coordinate
/// (`cycle`) cascades from the parent — we never re-declare it here.
/// Compose a phase scope's bindings source with SRD-75
/// phase-poll augmentation and/or phase-level `metrics:`
/// augmentation when applicable. Returns the existing
/// `BindingsDef` unchanged when the phase has neither `poll:`
/// nor `metrics:`; otherwise produces a
/// `BindingsDef::PolydatSource` that, for the poll case:
///
/// - **Prepends** `shared <name>: u64 := 0` for each
///   capture name declared by the phase's ops. The
///   shared-cell modifier means TraversingDispenser
///   writes via `ctx.wires.write` propagate to the
///   phase scope kernel (SRD-13c §"Implementation:
///   SharedCell-backed input slots" §4 "Write
///   through"); same cell is observable to other ops'
///   `if:` reads in the same iteration AND to the
///   `__poll_until` predicate evaluated on the phase
///   kernel.
/// - **Appends** the original `phase.bindings` body
///   verbatim (the author's bindings shadow synthesized
///   captures by ident if any name collides — same
///   shadowing rule as anywhere else in GK; we err out
///   in that case for clarity).
/// - **Appends** `__poll_until := <until>` as a regular
///   cycle binding (dynamic lifecycle per SRD-11 §"Two
///   Evaluation Lifecycles" — re-evaluates per pull as
///   capture cells update).
///
/// For the metrics case it appends an `extern phase_start: u64
/// = 0` origin wire (set by the executor at the completion-time
/// pull) and one `volatile __metric_<name> := <value>` per
/// declared phase metric. `volatile` on the `__metric_<name>`
/// binding excludes it from const-fold identity; a value that
/// reads a clock should declare that read as its own `volatile`
/// phase binding (e.g. `volatile now_ms := current_epoch_millis()`,
/// metric `value: now_ms - phase_start`) so the non-deterministic
/// node is acknowledged directly and the kernel compiles under
/// `--strict`. The executor pulls each `__metric_<name>` once at
/// phase completion and records it on the phase component.
///
/// Type inference is intentionally narrow in the
/// initial ship: every capture lands as `u64` (the
/// shape the canonical synchronizer-pattern workload
/// uses — `:count` reductions and numeric Jolokia
/// attribute reads). Non-numeric captures aren't
/// supported in the phase-poll predicate; the
/// workload author falls back to a different pattern.
/// SRD-75 §"Open questions: Capture-type inference
/// granularity" tracks the refinement.
pub fn synthesize_phase_scope_bindings(
    phase: &nmbrs_workload::model::WorkloadPhase,
) -> Result<nmbrs_workload::model::BindingsDef, String> {
    use nmbrs_workload::model::BindingsDef;
    let has_poll = phase.poll.is_some();
    let has_metrics = !phase.metrics.is_empty();
    // SRD-86 — an `optimize.objective` that is an inline expression (not a bare
    // wire reference) is lowered to a synthesized `__objective` binding below.
    let has_objective_expr = phase
        .optimize
        .as_ref()
        .is_some_and(|o| !objective_is_bare_wire(&o.objective));
    // No phase-scope augmentation needed — pass the author's bindings
    // through unchanged so a metrics/poll-free phase keeps its original
    // (possibly empty) `BindingsDef`. Stop conditions do NOT augment the
    // matter (they are scope-bound `ScopedExpr`s built against the phase
    // kernel), so they don't force synthesis here.
    if !has_poll && !has_metrics && !has_objective_expr {
        return Ok(phase.bindings.clone());
    }

    // Walk every op in the phase, collect declared
    // capture names. The capture set is the union across
    // ops in the phase (the synchronizer pattern's
    // common case: one read-op captures all the state,
    // a second trigger-op only reads via `if:`). Only
    // relevant to the poll augmentation; empty otherwise.
    let mut capture_names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if has_poll {
        for op in &phase.ops {
            for cap in &op.captures {
                if seen.insert(cap.as_name.clone()) {
                    capture_names.push(cap.as_name.clone());
                }
            }
        }
    }

    // Compose the augmented source.
    let mut source = String::new();
    if has_poll {
        source.push_str(
            "# SRD-75 phase-poll augmentation — synthesized.\n\
             # Captures land here as shared cells; ops write through via\n\
             # `ctx.wires.write` on their op-template kernel's import\n\
             # slots (Rule 1 shared import → cell attached at spawn).\n",
        );
        for name in &capture_names {
            // Polydat's `shared X := <literal>` form infers the type
            // from the RHS literal (per SRD-13c
            // §"Implementation: SharedCell-backed input slots"
            // — `shared X := 0` lands as u64 via literal-fold).
            // Captures default to u64 with init 0; numeric
            // predicates compare cleanly, and TraversingDispenser's
            // json_to_value coerces the response value into the
            // matching Value variant.
            source.push_str(&format!("shared {name} := 0\n"));
        }
    }

    let original_body: String = match &phase.bindings {
        BindingsDef::PolydatSource(s) => s.clone(),
        BindingsDef::Map(m) => {
            let mut out = String::new();
            for (n, e) in m {
                out.push_str(&format!("{n} := {e}\n"));
            }
            out
        }
    };
    if !original_body.trim().is_empty() {
        // Detect collisions between author-declared
        // bindings and synthesized captures. A collision
        // is most likely a workload bug (the author
        // didn't realize the name was being synthesized
        // by phase-poll); fail loudly per SRD-30 unknown-
        // field hygiene rather than silently letting one
        // win.
        let locally_declared = scan_locally_declared_idents(&original_body);
        for name in &capture_names {
            if locally_declared.contains(name) {
                return Err(format!(
                    "phase-poll synthesis: capture name '{name}' is also \
                     declared by the phase's `bindings:` block — pick one. \
                     SRD-75 synthesizes captures as `shared <name>: u64`; \
                     re-declaring the same name in bindings is a collision."
                ));
            }
        }
        source.push_str(&original_body);
        if !source.ends_with('\n') {
            source.push('\n');
        }
    }

    if has_poll {
        // The predicate is a regular cycle binding (NOT
        // `const` — its upstream depends on per-cycle
        // capture writes, so it must re-evaluate per pull).
        let poll = phase.poll.as_ref().expect("has_poll");
        source.push_str(&format!("__poll_until := {}\n", poll.until));
    }

    if has_metrics {
        // Phase-level `metrics:` — emit one `volatile
        // __metric_<name> := <value>` per declared metric, plus the
        // executor-injected `phase_start` origin wire. `volatile`
        // acknowledges the nondeterminism of clock-reading values
        // like `phase_elapsed(phase_start)` so the phase kernel
        // compiles in strict mode (and excludes them from const-fold
        // identity); for a deterministic value it is harmless. The
        // executor pulls each `__metric_<name>` once at phase
        // completion — see `executor::emit_phase_metrics`.
        // `phase_start` binds the runtime's phase-scoped clock node rather
        // than an extern the executor has to remember to fill. As an extern it
        // was set ONLY on the completion-time metric pull, on a fresh subscope
        // — so anything reading it while the phase was still running (an op,
        // most obviously) saw the declared default of 0 and silently computed
        // against the epoch. The node reads the phase origin the executor
        // scopes in `run_phase`, so the value is correct from the phase's
        // first op onward, and `volatile` keeps it out of const-fold identity.
        source.push_str(
            "# Phase-level metrics — synthesized.\n\
             # `phase_start` is the epoch millis at which THIS phase started,\n\
             # read from the runtime's phase-scoped clock.\n\
             volatile phase_start := phase_start_millis()\n",
        );
        let mut entries: Vec<_> = phase.metrics.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, spec) in entries {
            let binding = synthesize_metric_binding_name(name);
            source.push_str(&format!(
                "volatile {binding} := {expr}\n",
                expr = spec.value
            ));
            // See the op-template path: a coordinate is compiled matter, not a
            // runtime string. `volatile` for the same reason the value is —
            // a coordinate read from a capture or a clock must not be folded
            // into one phase's value.
            for (dim, expr) in &spec.cell {
                let cell_binding = synthesize_cell_binding_name(name, dim);
                source.push_str(&format!("volatile {cell_binding} := {expr}\n"));
            }
        }
    }

    if has_objective_expr {
        // SRD-86 — lower an inline `optimize.objective` expression to one wire
        // the optimizer reads (`__objective`), so an author can write the
        // objective inline instead of pre-declaring a `bindings:` entry.
        // Emitted LAST so it can reference any author binding or synthesized
        // `__metric_<name>` above it. `volatile` lets a metricsql/clock-reading
        // objective compile under `--strict` and is harmless for a
        // deterministic expression; the settle-vs-one-shot routing keys off
        // program-wide reader-node presence, not this flag, so a deterministic
        // inline objective still takes the one-shot path.
        let objective = &phase
            .optimize
            .as_ref()
            .expect("has_objective_expr")
            .objective;
        source.push_str(&format!(
            "# SRD-86 inline objective — synthesized.\n\
             volatile {OBJECTIVE_WIRE} := {objective}\n",
        ));
    }

    // SRD-83 stop conditions are NOT synthesized into the phase matter.
    // Each predicate is compiled as a scope-bound `ScopedExpr`
    // (`stop_conditions::compile_stop_condition`) against the *built*
    // phase kernel and evaluated per trigger — authored phase bindings
    // and evaluated stop predicates stay orthogonal concerns.

    Ok(BindingsDef::PolydatSource(source))
}

/// Synthesize a phase-scope kernel.
///
/// Pushes the phase's own `bindings:` body verbatim, then drives
/// the shared cascade walker to declare every parent-visible
/// name the body references plus the standard workload-param +
/// parent-output + parent-input cascade.
// reason: cohesive scope-kernel constructor — each argument is a distinct
// piece of the scope's compile context (bindings, parent kernel, params,
// cascade inputs); a parameter struct would only relocate the same fields.
#[allow(clippy::too_many_arguments)]
pub fn build_phase_scope_kernel(
    bindings: &nmbrs_workload::model::BindingsDef,
    parent_manifest: &[crate::runner::ManifestEntry],
    parent_kernel: &polydat::kernel::PolydatKernel,
    workload_params: &HashMap<String, String>,
    polydat_lib_paths: Vec<std::path::PathBuf>,
    workload_dir: Option<&std::path::Path>,
    strict: bool,
    context: &str,
) -> Result<polydat::kernel::PolydatKernel, String> {
    use nmbrs_workload::model::BindingsDef;

    let body_text: String = match bindings {
        BindingsDef::PolydatSource(s) => s.clone(),
        BindingsDef::Map(m) => {
            let mut out = String::new();
            for (name, expr) in m {
                out.push_str(&format!("{name} := {expr}\n"));
            }
            out
        }
    };

    let mut source = String::new();
    let mut emitted: HashSet<String> = HashSet::new();
    let mut inherited_names: Vec<String> = Vec::new();

    // Discover the names the phase body references and the
    // names it locally declares (which shadow parent-output
    // cascade per SRD-13f).
    let body_locally_declared = scan_locally_declared_idents(&body_text);
    let mut referenced: HashSet<String> = HashSet::new();
    collect_string_interp_refs(&body_text, &mut referenced);
    for ident in scan_idents_in_polydat_source(&body_text) {
        if !body_locally_declared.contains(&ident) {
            referenced.insert(ident);
        }
    }

    // Drive the shared cascade walker. The phase body's
    // locally-declared idents go into both pre_emitted (so the
    // walker doesn't re-cascade them) and shadow_names (so the
    // workload-param + parent-output passes also skip them).
    crate::scope_synth::cascade_parent_into_source(
        crate::scope_synth::CascadeInputs {
            parent_kernel,
            workload_params,
            parent_manifest,
            referenced: &referenced,
            pre_emitted: &body_locally_declared,
            shadow_names: &body_locally_declared,
            // Phase opts out: the body may already declare
            // referenced names as `input` / `extern` and a
            // step-3 emission would collide. Phase relies on
            // step 5 (parent-output cascade) and step 6
            // (parent-input cascade) to bring in what the body
            // doesn't declare locally.
            include_referenced_cascade: false,
        },
        crate::scope_synth::CascadeOutputs {
            source: &mut source,
            emitted: &mut emitted,
            inherited_names: &mut inherited_names,
        },
    );

    // Append the phase's own bindings body verbatim. The GK
    // compiler classifies each statement (init / shared / final)
    // per the standard rules.
    if !source.ends_with('\n') && !source.is_empty() {
        source.push('\n');
    }
    source.push_str(&body_text);
    if !source.ends_with('\n') {
        source.push('\n');
    }

    let compile_options = polydat::kernel::subcontext::CompileOptions {
        workload_dir: workload_dir.map(|p| p.to_path_buf()),
        polydat_lib_paths,
        strict,
        required_outputs: Vec::new(),
        context_label: Some(context.to_string()),
        cursor_limit: None,
        ..Default::default()
    };

    if std::env::var("NMBRS_DEBUG_SCOPE_SYNTH")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        eprintln!(
            "=== SCOPE SYNTH [{context}] inherited={inherited_names:?} ===\n{source}\n=== END ==="
        );
    }
    let matter = polydat::kernel::subcontext::PolydatMatter::builder()
        .label(context)
        .source(source)
        .inherited_outputs(inherited_names)
        .options(compile_options)
        .build()
        .map_err(|e| format!("{context}: phase scope synthesis: {e}"))?;
    let mut kernel = parent_kernel
        .build_subscope(matter)
        .map_err(|e| format!("{context}: phase scope synthesis: {e}"))?;
    parent_kernel.propagate_inputs_into(&mut kernel);
    Ok(kernel)
}

// reason: cohesive scope-kernel constructor — each argument is a distinct
// piece of the do-loop's compile context (counter, condition, parent kernel,
// params, cascade inputs); a parameter struct would only relocate the fields.
#[allow(clippy::too_many_arguments)]
pub fn build_do_loop_scope_kernel(
    counter: Option<&str>,
    condition: &str,
    parent_manifest: &[crate::runner::ManifestEntry],
    parent_kernel: &polydat::kernel::PolydatKernel,
    workload_params: &HashMap<String, String>,
    polydat_lib_paths: Vec<std::path::PathBuf>,
    workload_dir: Option<&std::path::Path>,
    strict: bool,
    context: &str,
) -> Result<polydat::kernel::PolydatKernel, String> {
    // Scope-specific contribution: declare the counter extern
    // (if a counter is in play). The counter is the do-loop's
    // only own-iter wire; pre-emit it so the shared cascade
    // walker doesn't try to type-cascade it from the parent
    // (which doesn't know about it).
    let mut source = String::new();
    let mut emitted: HashSet<String> = HashSet::new();
    let mut inherited_names: Vec<String> = Vec::new();

    if let Some(c) = counter {
        source.push_str(&format!("extern {c}: u64\n"));
        emitted.insert(c.to_string());
    }

    // Drive the shared cascade walker. `referenced` is the set
    // of `{name}` placeholders the condition expression
    // mentions; pre_emitted is the counter (if any).
    let referenced = collect_leaf_placeholders(&[condition.to_string()]);
    let pre_emitted: HashSet<String> = counter.iter().map(|c| c.to_string()).collect();
    let shadow_names: HashSet<String> = pre_emitted.clone();
    crate::scope_synth::cascade_parent_into_source(
        crate::scope_synth::CascadeInputs {
            parent_kernel,
            workload_params,
            parent_manifest,
            referenced: &referenced,
            pre_emitted: &pre_emitted,
            shadow_names: &shadow_names,
            // do-loop opts in: the condition expression is
            // narrowly-scoped Polydat source that references
            // names which need extern declarations to be
            // resolvable. Step 3 emits those externs against
            // parent_manifest's types.
            include_referenced_cascade: true,
        },
        crate::scope_synth::CascadeOutputs {
            source: &mut source,
            emitted: &mut emitted,
            inherited_names: &mut inherited_names,
        },
    );

    if source.is_empty() {
        source.push_str("const __empty := 0\n");
    }

    // SRD-67 — finalize through the SubcontextBuilder bridge.
    // `polydat_lib_paths` / `workload_dir` / `strict` aren't yet
    // threaded through (the bridge uses the default `compile_ast`
    // path); the do-loop's emitted source shape doesn't need
    // them. Recorded as a Phase 3 follow-up.
    let _ = (polydat_lib_paths, workload_dir, strict);
    let matter = polydat::kernel::subcontext::PolydatMatter::builder()
        .label(context)
        .source(source)
        .inherited_outputs(inherited_names)
        .build()
        .map_err(|e| format!("{context}: do-loop scope synthesis: {e}"))?;
    let mut kernel = parent_kernel
        .build_subscope(matter)
        .map_err(|e| format!("{context}: do-loop scope synthesis: {e}"))?;
    parent_kernel.propagate_inputs_into(&mut kernel);
    Ok(kernel)
}

/// Token-shaped identifier scan over Polydat source. Returns every
/// alphanumeric/underscore-shaped token that isn't a keyword
/// or numeric literal. Used by
/// [`build_op_template_scope_kernel`] to discover names the
/// op's bindings body references; the Polydat compiler does the
/// authoritative parse downstream — this scan is just a
/// best-effort first pass for the cross-scope contract check.
pub(crate) fn scan_idents_in_polydat_source(src: &str) -> HashSet<String> {
    const KEYWORDS: &[&str] = &[
        "input", "extern", "const", "final", "init", "shared", "volatile", "cursor", "pragma",
        "true", "false", "as", "in", "for",
        // Block-form conditional selection: `if <cond> { a } else { b }`. This is a
        // TEXTUAL scan, so it cannot tell a keyword from a wire name by position —
        // before these were listed, a binding using the block form reported `if` and
        // `else` as unresolved wire references. The call form `if(cond, a, b)` was
        // unaffected and so did not surface this.
        "if", "else",
    ];
    let mut out = HashSet::new();
    let mut chars = src.chars().peekable();
    let mut current = String::new();
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    // Suppress the next ident — set when we just saw a `:` outside
    // strings/comments. Lets us skip the type token in declarations
    // like `extern x: u64`, `input cycle: u64`, `input (a: u64, b: f64)`,
    // and module signatures `foo(p: u64) -> (q: f64)`. A type name
    // (`u64`/`f64`/`Str`/etc.) is not a wire reference.
    let mut suppress_next_ident = false;
    while let Some(c) = chars.next() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }
        if in_block_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }
        if in_string {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            continue;
        }
        // `#` to end-of-line — YAML-style hash comment, matching the
        // Polydat lexer (`dsl/lexer.rs` "Skip hash comments"). Without
        // this, comment words inside a `bindings: |` block leak in as
        // phantom wire references.
        if c == '#' {
            in_line_comment = true;
            continue;
        }
        if c == '/' {
            if chars.peek() == Some(&'/') {
                chars.next();
                in_line_comment = true;
                continue;
            }
            if chars.peek() == Some(&'*') {
                chars.next();
                in_block_comment = true;
                continue;
            }
        }
        if c.is_alphanumeric() || c == '_' {
            current.push(c);
        } else if !current.is_empty() {
            // Token boundary — is `current` an ident?
            let is_ident = !current
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(true)
                && !KEYWORDS.contains(&current.as_str());
            if is_ident && !suppress_next_ident {
                out.insert(current.clone());
            }
            current.clear();
            suppress_next_ident = false;
        }
        // Track `:` separately so the next ident is treated as a
        // type annotation. Excludes `:=` (the binding operator) by
        // peeking ahead.
        if c == ':' && chars.peek() != Some(&'=') {
            suppress_next_ident = true;
        }
    }
    if !current.is_empty()
        && !current
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(true)
        && !KEYWORDS.contains(&current.as_str())
        && !suppress_next_ident
    {
        out.insert(current);
    }
    out
}

/// Names declared on the LHS of `:=` (or `=` for init bindings)
/// in Polydat source. Locally-declared names shadow parent-scope
/// references per SRD-13c §"Shadowing", so the cross-scope
/// contract check skips them.
pub(crate) fn scan_locally_declared_idents(src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in src.lines() {
        let line = line.trim();
        // Skip blanks + line comments. `#` (YAML-style) and `//` are
        // both Polydat line-comment forms (`dsl/lexer.rs`); neither
        // declares a binding.
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        // Drop modifier prefixes (`final const x := …`,
        // `shared y := …`) so the LHS ident is always the
        // last word before `:=` or `=`.
        let prefixes = [
            "shared const ",
            "const ",
            "init ",
            "shared ",
            "final ",
            "volatile ",
            "extern ",
        ];
        let mut rest = line;
        loop {
            let mut stripped = false;
            for p in &prefixes {
                if let Some(r) = rest.strip_prefix(p) {
                    rest = r.trim_start();
                    stripped = true;
                    break;
                }
            }
            if !stripped {
                break;
            }
        }
        // Find `:=` or `=` (but not `==`).
        let assign_idx = rest.find(":=").or_else(|| {
            rest.find('=')
                .filter(|&i| rest.as_bytes().get(i + 1) != Some(&b'='))
        });
        let Some(idx) = assign_idx else { continue };
        let lhs = rest[..idx].trim();
        // `extern name: type` — the lhs is "name: type"; strip the type.
        let name = lhs.split(':').next().unwrap_or(lhs).trim();
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.insert(name.to_string());
        }
    }
    out
}

// SRD-13c `ParentRefKind` classification and the
// `classify_parent_ref` helper were retired by SRD-13f. The
// snapshot-vs-shared contract they enforced is superseded by
// uniform construction-time wiring + per-cycle refresh on
// non-shared parent outputs (SRD-13f §"Materialization gradient").

/// SRD-13d Phase 9 — synthesize a per-op-template kernel for a
/// materialised op-template scope.
///
/// Mirrors [`build_do_loop_scope_kernel`] but uses the op's
/// `bindings:` block (which already carries metric `:=` injections
/// per SRD-40b §1) as the body. The resulting kernel:
///
/// 1. Emits an `extern <name>: <type>` for every parent-visible
///    name the op explicitly references — and only those, so
///    the op-template kernel stays narrow.
/// 2. Validates each reference against the SRD-13c cross-scope
///    visibility contract: `Input`, `SharedOutput`, or
///    `ConstantOutput` are accepted; `DynamicOutput` errors at
///    workload-init time with a clear message pointing at the
///    `shared` modifier path.
/// 3. Emits the op's own bindings.
/// 4. Compiles + `materialize_wiring_from_outer`s to the parent kernel and
///    propagates inherited inputs.
// reason: cohesive scope-kernel constructor — each argument is a distinct
// piece of the op-template's compile context (op, parent kernel, params,
// cascade inputs); a parameter struct would only relocate the same fields.
#[allow(clippy::too_many_arguments)]
pub fn build_op_template_scope_kernel(
    op: &nmbrs_workload::model::ParsedOp,
    parent_manifest: &[crate::runner::ManifestEntry],
    parent_kernel: &polydat::kernel::PolydatKernel,
    workload_params: &HashMap<String, String>,
    polydat_lib_paths: Vec<std::path::PathBuf>,
    workload_dir: Option<&std::path::Path>,
    strict: bool,
    kernel_opt: polydat::kernel::KernelOptLevel,
    context: &str,
) -> Result<polydat::kernel::PolydatKernel, String> {
    use nmbrs_workload::model::BindingsDef;

    let manifest_by_name: HashMap<&str, &crate::runner::ManifestEntry> = parent_manifest
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let mut source = String::new();
    let mut emitted: HashSet<String> = HashSet::new();
    let mut inherited_names: Vec<String> = Vec::new();

    // SRD-13f: every parent-visible wire the op template
    // references — whether in body, condition, delay, metric
    // values, op fields (`stmt`, `uri`, `body`, etc.), or
    // string-interpolation arguments inside the body — gets
    // wired into the local op-template kernel at construction.
    // Local reads on the op-template kernel then resolve every
    // such name through the local read API, with construction-
    // time wiring guaranteeing the read invariant (inner reads
    // return the value that outer would return — see SRD-13f
    // §"The read invariant"). The wires layer takes one kernel
    // handle and never composes chains externally.
    let body_text: String = match &op.bindings {
        BindingsDef::PolydatSource(s) => s.clone(),
        BindingsDef::Map(m) => {
            let mut out = String::new();
            for (name, expr) in m {
                out.push_str(&format!("{name} := {expr}\n"));
            }
            out
        }
    };

    // SRD-13f Push D: declare the parent's coordinate inputs
    // (typically just `cycle`) on this op-template kernel.
    // Pre-Push-D, the workload-level `input <name>: <type>` line
    // was merged into op.bindings and arrived via body_text, so
    // the kernel always had a Coordinate slot. After Push D
    // body_text no longer carries it, so we emit the declaration
    // explicitly — without it the op-template kernel has no
    // input slot for cycle and the runtime's per-cycle
    // `set_inputs` writes go nowhere.
    let body_has_inputs_decl = body_text
        .lines()
        .any(|line| line.trim_start().starts_with("input "));
    if !body_has_inputs_decl {
        let parent_coord_names: Vec<String> = parent_kernel
            .program()
            .input_names()
            .into_iter()
            .take(parent_kernel.program().coord_count())
            .collect();
        if !parent_coord_names.is_empty() {
            source.push_str(&format_input_decl_line(&parent_coord_names));
            for name in &parent_coord_names {
                emitted.insert(name.clone());
            }
        }
    }
    // SRD-109 Part 3 — `results:` interface wires are dispenser-
    // delivered projections (`result:` path entries evaluated by
    // the result wrapper); declare each as an explicit extern at
    // its interface type so the per-cycle `ctx.wires.write` lands
    // on a correctly-typed slot. Emitted before the cascade scan
    // so a relevancy/metric reference to the same name doesn't
    // try to pull it from the parent.
    if let Some(iface) = op.abstract_interface.as_ref() {
        for (rname, kw) in &iface.results {
            if polydat::ast::PortType::from_keyword(kw).is_none() {
                return Err(format!(
                    "{context}: interface results wire '{rname}' declares \
                     unknown type '{kw}'"
                ));
            }
            if !emitted.contains(rname) {
                source.push_str(&format!("extern {rname}: {kw}\n"));
                emitted.insert(rname.clone());
            }
        }
    }

    let body_idents = scan_idents_in_polydat_source(&body_text);
    let body_locally_declared = scan_locally_declared_idents(&body_text);
    let mut referenced: Vec<String> = Vec::new();
    // Op field references — `stmt`, `uri`, `body`, etc. carry
    // `{name}` placeholders the dispenser substitutes at cycle
    // time. Those wires must be reachable on the op-template
    // kernel so the dispenser's single-kernel-handle wires
    // surface resolves them through the local read API.
    for value in op.op.values() {
        if let Some(s) = value.as_str() {
            for n in nmbrs_workload::bindpoints::referenced_bindings(s) {
                if !body_locally_declared.contains(&n) && !referenced.iter().any(|r| r == &n) {
                    referenced.push(n);
                }
            }
        }
    }
    for ident in &body_idents {
        if body_locally_declared.contains(ident) {
            continue;
        }
        if !referenced.iter().any(|r| r == ident) {
            referenced.push(ident.clone());
        }
    }
    // String-interpolation references inside the body (e.g.
    // `dataset_prebuffer("{dataset}:{profile}")`). The Polydat compiler
    // desugars those into wires that need a matching extern;
    // without this scan, the cascade misses them and the compiler
    // defaults the auto-extern to u64, landing a Str value into a
    // u64 slot at bind-time and panicking via an inserted
    // `__u64_to_string` adapter.
    let mut interp_refs: HashSet<String> = HashSet::new();
    collect_string_interp_refs(&body_text, &mut interp_refs);
    for ident in interp_refs {
        if body_locally_declared.contains(&ident) {
            continue;
        }
        if !referenced.iter().any(|r| r == &ident) {
            referenced.push(ident);
        }
    }
    if let Some(ref s) = op.condition {
        let n = s.trim().trim_start_matches('{').trim_end_matches('}');
        if !n.is_empty() && !body_locally_declared.contains(n) && !referenced.iter().any(|r| r == n)
        {
            referenced.push(n.to_string());
        }
    }
    // Capture targets are a WRITE usage surface of this op: the
    // runtime lands each captured value on this kernel's input slot
    // via `ctx.wires.write` (see TraversingDispenser). The name must
    // therefore be declared here like any other referenced wire —
    // the standard cascade below emits the `extern` (with the
    // parent's port type) and the wiring machinery binds it to the
    // ancestral `shared` cell when one is in scope, making the
    // write visible across phases. Without the declaration a
    // capture-only op's write is a silent NoSlot drop, and any
    // later phase gating on the cell reads its initializer forever.
    for cap in &op.captures {
        let n = cap.as_name.as_str();
        if !n.is_empty() && !body_locally_declared.contains(n) && !referenced.iter().any(|r| r == n)
        {
            referenced.push(n.to_string());
        }
    }
    if let Some(ref delay_spec) = op.delay {
        for raw in delay_spec.names() {
            let n = raw.trim().trim_start_matches('{').trim_end_matches('}');
            if !n.is_empty()
                && !body_locally_declared.contains(n)
                && !referenced.iter().any(|r| r == n)
            {
                referenced.push(n.to_string());
            }
        }
    }
    for spec in op.metrics.values() {
        let trimmed = spec.value.trim();
        let bare = !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_');
        if bare
            && !body_locally_declared.contains(trimmed)
            && !referenced.iter().any(|r| r == trimmed)
        {
            referenced.push(trimmed.to_string());
        }
    }

    // Wire references inside the relevancy block. The validator
    // resolves `actual:`, `expected:`, `k:`, `r:` at wrap-time
    // through the dispenser's canonical kernel (parse_count_param
    // and the `expected_wire_name` path in `validation.rs`).
    // Strings that are bare identifiers OR `{name}` text-templates
    // name a wire that must be visible on this op-template kernel;
    // numeric / integer-literal values aren't wire references and
    // pass through.
    if let Some(rel) = op.params.get("relevancy").and_then(|v| v.as_object()) {
        for key in &["actual", "expected", "k", "r"] {
            let Some(val) = rel.get(*key).and_then(|v| v.as_str()) else {
                continue;
            };
            let trimmed = val
                .trim()
                .trim_start_matches('{')
                .trim_end_matches('}')
                .trim();
            if trimmed.is_empty() {
                continue;
            }
            // Skip numeric literals — `k: 10` is a constant, not a
            // wire ref.
            if trimmed.parse::<i64>().is_ok() {
                continue;
            }
            // Bare-identifier check matches the validator's
            // `is_bare_ident` rule.
            let bare = trimmed
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_')
                .unwrap_or(false)
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !bare {
                continue;
            }
            if !body_locally_declared.contains(trimmed) && !referenced.iter().any(|r| r == trimmed)
            {
                referenced.push(trimmed.to_string());
            }
        }
    }

    // SRD-66 result-bindings: every LHS name a result-binding
    // declares needs an input slot on this op-template kernel so
    // the Rule 2 write-through can wire to the parent's SHARED
    // cell. Without this, the cell-bound slot is never declared,
    // `add_result_bindings` falls back to PortType::U64 (the
    // "couldn't classify" default) for the export, and the write-
    // through stores into a U64-typed view of what's actually a
    // Bool/Str/etc. cell — surfaces downstream as a "non-bool
    // type" pick panic.
    //
    // String-shape fragments carry full Polydat source whose LHS we
    // extract via `scan_locally_declared_idents`. Map-shape
    // fragments give the name directly.
    if let Some(rb) = op.result.as_ref() {
        rb.walk_fragments(|frag| match frag {
            nmbrs_workload::model::ResultFragment::Source(s) => {
                for n in scan_locally_declared_idents(s) {
                    if !body_locally_declared.contains(&n) && !referenced.iter().any(|r| r == &n) {
                        referenced.push(n);
                    }
                }
            }
            nmbrs_workload::model::ResultFragment::Named { name, .. } => {
                let n = name.to_string();
                if !body_locally_declared.contains(&n) && !referenced.iter().any(|r| r == &n) {
                    referenced.push(n);
                }
            }
        });
    }

    // SRD-13f: parent-ref classification no longer gates
    // extern emission. The legacy "DynamicOutput without
    // shared" rejection assumed snapshot semantics for
    // non-shared cross-scope reads (SRD-13c §"Default:
    // Immutable Propagation"). SRD-13f reframes that rule:
    // the read invariant is uniform across all visible
    // parent outputs — inner reads return outer's current
    // value — and the construction-time wiring (cell
    // attachment for shared, per-cycle refresh for non-shared
    // pending the full cell mechanism in Push B.2) keeps the
    // invariant intact. No external pre-check needed.

    // Emit extern decls only for names the op references and
    // that the parent provides. Lazy cascade — keeps the
    // op-template kernel narrow and makes the contract check
    // above crisp.
    for name in &referenced {
        if emitted.contains(name) {
            continue;
        }
        if body_locally_declared.contains(name) {
            continue;
        }
        // Names that are *Coordinate* inputs in the parent (the
        // implicit `cycle` and friends) must stay Coordinate in
        // the inner kernel too, so `set_inputs` propagates them
        // per cycle. An explicit `extern` declaration would force
        // IterationExtern classification and break propagation —
        // skip the explicit emit and let the inner kernel's auto-
        // extern path re-classify as Coordinate.
        let is_parent_coord = parent_kernel
            .program()
            .find_input(name)
            .and_then(|idx| parent_kernel.program().input_kind(idx))
            .is_some_and(|k| matches!(k, polydat::kernel::InputKind::Coordinate));
        if is_parent_coord {
            // The cascade still needs to record this as an
            // inherited name so `mark_inherited_outputs` includes
            // it — the inner kernel will re-publish it as an
            // (auto-extern) input/output and materialize_wiring_from_outer
            // will value-copy at construction time.
            inherited_names.push(name.clone());
            continue;
        }
        // Workload-param check goes BEFORE manifest cascade: a
        // workload param ALSO appears in the parent's manifest
        // (cascaded as an auto-output of an `extern <name>: …`
        // line in the for_each scope synthesiser), but op-template
        // bodies need it as a `final` literal so `init <name> =
        // <expr-using-param>` folds at compile time. Routing
        // through the manifest path emits `extern`, leaves init
        // unfolded, and the runtime sees `Value::None` in the
        // input slot — exactly the surface the Phase-9 op-template
        // kernel was hitting on dataset_prebuffer / query_count.
        if let Some(value) = workload_params.get(name) {
            // Chain-aware: a `set:` / `bindings:` scope above
            // us may have shadowed the workload-param default.
            // This was the root cause of the CQL prepared-
            // statement regression where `{ann_opts}` got
            // emitted as a `?` bind point — the op-template
            // kernel's local `const rerank_mode := "default"`
            // masked the SetParam shadow, and the downstream
            // `const ann_opts := select_str(str_eq(
            // rerank_mode, "pinned"), …)` folded against the
            // wrong rerank_mode value.
            crate::scope_synth::emit_workload_param_chain_aware(
                name,
                value,
                parent_kernel,
                &mut source,
                &mut emitted,
                None,
            );
        } else if let Some(entry) = manifest_by_name.get(name.as_str()) {
            let type_name = entry.port_type.to_keyword();
            source.push_str(&format!("extern {name}: {type_name}\n"));
            emitted.insert(name.clone());
            inherited_names.push(name.clone());
        } else if let Some(parent_idx) = parent_kernel.program().find_input(name) {
            // Parent INPUT — for non-Coordinate inputs (iteration
            // vars, external-write ports) emit an explicit `extern` so the
            // inner kernel's input classification matches the
            // parent's, and materialize_wiring_from_outer can value-copy or
            // shared-cell-attach. For Coordinate inputs (e.g. the
            // implicit `cycle` coord), DON'T emit — let the GK
            // auto-extern path classify them as Coordinate too,
            // so per-cycle `set_inputs` propagates to the inner
            // kernel. An explicit `extern` would force
            // IterationExtern and break that propagation.
            let kind = parent_kernel.program().input_kind(parent_idx);
            if !matches!(kind, Some(polydat::kernel::InputKind::Coordinate)) {
                let port_type = parent_kernel
                    .program()
                    .input_port_type(name)
                    .ok_or_else(|| {
                        format!(
                            "scope synthesis: parent input '{name}' has no \
                         declared PortType (find_input returned index {parent_idx} \
                         but input_port_type returned None — kernel program shape \
                         broken)"
                        )
                    })
                    .expect("input index just resolved must have a port type");
                let type_name = port_type.to_keyword();
                source.push_str(&format!("extern {name}: {type_name}\n"));
                emitted.insert(name.clone());
                inherited_names.push(name.clone());
            }
        }
    }

    // SRD-13f Push A: workload params are root-level context
    // wires — they must appear on every scope's kernel program
    // as inlined constants (materialization gradient §"Inlined
    // constant"). The per-name loop above emits them as `final`
    // for body-referenced params; this cascade catches the rest
    // so every workload param lands on this op-template kernel
    // as a folded constant. The previous emission shape
    // (`extern X: type`) was wrong — it forced a runtime input
    // slot, which (a) prevented `const` bindings from folding
    // against the param's value at compile time and (b) routed
    // a compile-time-constant value through the runtime input
    // path. `final` is the correct materialization.
    for (name, value) in workload_params {
        // Chain-aware emission: honors any `set:` / `bindings:`
        // scope shadow above us. Without it, the op-template's
        // local `const NAME := <hashmap-default>` would mask
        // the shadow and break SRD-21's single-resolution-
        // surface invariant.
        crate::scope_synth::emit_workload_param_chain_aware(
            name,
            value,
            parent_kernel,
            &mut source,
            &mut emitted,
            None,
        );
    }
    if body_text.trim().is_empty() {
        // No own bindings — the kernel just re-exports parent.
        // The flatten-elision logic upstream should have caught
        // this, but defensively keep the kernel non-empty.
        if source.is_empty() {
            source.push_str("const __empty := 0\n");
        }
    } else {
        if !source.ends_with('\n') && !source.is_empty() {
            source.push('\n');
        }
        source.push_str(&body_text);
        if !source.ends_with('\n') {
            source.push('\n');
        }
    }

    // SRD-67 Phase 3 — route op-template scope synthesis through
    // the SubcontextBuilder bridge. The Phase-9 op-template
    // kernel needs the same `compile_polydat_with_libs` knobs the
    // legacy direct call took (lib paths, strict, source dir,
    // context label); those flow through `CompileOptions`. The
    // bridge applies `mark_inherited_outputs` and
    // `materialize_wiring_from_outer` against the live parent so per-cycle
    // values reach the inner kernel's input slots; the trailing
    // `PolydatKernel::propagate_inputs_into` keeps cascade-extern'd inputs
    // flowing through (until Rule 4 / Rule 5 absorb them).
    let compile_options = polydat::kernel::subcontext::CompileOptions {
        workload_dir: workload_dir.map(|p| p.to_path_buf()),
        polydat_lib_paths,
        strict,
        required_outputs: Vec::new(),
        context_label: Some(context.to_string()),
        cursor_limit: None,
        kernel_opt,
    };

    // SRD-67 Phase 5 — fold the SRD-66 `result:` source through
    // `add_result_bindings`. The builder walks the source's free
    // identifiers, injects magic externs (`body` / `count` /
    // `ok`) it actually references, and registers each LHS as
    // an export. Rule 2 in finalize rewrites any LHS that
    // collides with a parent `shared` export into a write-
    // through; the kernel carries the bindings forward via
    // `set_write_throughs` so the per-cycle dispenser can
    // commit. Map-shape entries (already flattened to
    // `<name> := <source>` in the workload model) flow
    // through unchanged; path expressions surface as unbound-
    // identifier compile errors with the SRD-66 deferred-
    // structural-body-wire diagnostic.
    // SRD-66 result bindings + post-SRD-68 metric-value bindings.
    // Both routes feed the same closure-binding-economy walker
    // (`add_result_bindings`) which injects magic externs
    // (body/count/ok) for any of those names referenced from
    // either RHS. The walker then registers each LHS as a kernel
    // output the metrics wrapper / evaluator reads at cycle time
    // via `wires.get`.
    //
    // Metric-driven bindings use the `__metric_<name>` prefix so
    // they don't collide with user-declared output names and so
    // diagnostic output can filter them as internal. MetricsDispenser
    // reads the same `__metric_<name>` form at cycle time —
    // `synthesize_metric_binding_name` is the single source of
    // truth for the naming convention.
    let mut result_source: String = op
        .result
        .as_ref()
        .map(collect_result_bindings_source)
        .unwrap_or_default();
    if !op.metrics.is_empty() {
        // Stable ordering — matches `MetricsDispenser::wrap`'s
        // sort-by-name so synthesis order is reproducible.
        let mut entries: Vec<_> = op.metrics.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, spec) in entries {
            let binding = synthesize_metric_binding_name(name);
            result_source.push_str(&format!("{binding} := {expr}\n", expr = spec.value));
            // Cell coordinates are reified the same way the value is: as
            // compiled kernel bindings walked by `add_result_bindings`, so a
            // coordinate expression gets its magic-extern slots and is
            // type-checked, rather than being a string assembled at runtime.
            // `BTreeMap` iteration keeps the emission order stable.
            for (dim, expr) in &spec.cell {
                let cell_binding = synthesize_cell_binding_name(name, dim);
                result_source.push_str(&format!("{cell_binding} := {expr}\n"));
            }
        }
    }
    // Poll predicate. `poll.until:` lowers to the SAME binding name the
    // phase-level poll uses (`condition::UNTIL_BINDING`), in the op's own
    // kernel — so the poll wrapper reads one wire and never asks which kind of
    // node it is running under. Ordinary scoping does the rest: an op's
    // predicate shadows its phase's, as any other wire would.
    if let Some(until) = template_poll_until(op) {
        result_source.push_str(&format!(
            "{} := {until}\n",
            crate::wrappers::condition::UNTIL_BINDING,
        ));
    }
    // While-wrapper predicate. Synthesised as `__while := <expr>`
    // so `add_result_bindings` walks the expression, injects
    // magic externs for any captured wires it references, and
    // registers `__while` as a kernel output the WhileWrapper
    // reads at cycle time via the canonical pull plan.
    if let Some(while_expr) = op.while_cond.as_ref() {
        result_source.push_str(&format!(
            "{} := {expr}\n",
            crate::wrappers::r#while::BINDING_NAME,
            expr = while_expr,
        ));
    }
    let result_source: Option<String> = Some(result_source).filter(|s| !s.trim().is_empty());

    if std::env::var("NMBRS_DEBUG_SCOPE_SYNTH")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        eprintln!(
            "=== OP SYNTH [{context}] inherited={inherited_names:?} parent_has_xval_input={:?} parent_manifest_has_xval={} ===\n{source}\n=== END ===",
            parent_kernel.program().find_input("xval"),
            manifest_by_name.contains_key("xval")
        );
    }
    let mut matter_builder = polydat::kernel::subcontext::PolydatMatter::builder()
        .label(context)
        .source(source)
        .inherited_outputs(inherited_names)
        .options(compile_options);
    if let Some(rb) = result_source {
        matter_builder = matter_builder.result_bindings(rb);
    }
    let matter = matter_builder
        .build()
        .map_err(|e| format!("{context}: op-template scope synthesis: {e}"))?;
    let mut kernel = parent_kernel
        .build_subscope(matter)
        .map_err(|e| format!("{context}: op-template scope synthesis: {e}"))?;
    parent_kernel.propagate_inputs_into(&mut kernel);

    // SRD-108 Part B — the interface's TYPE PROOF, at the same
    // place every other wire is type-checked: pre-map synthesis,
    // against the compiled op-template program. Load-time binder
    // checks were the early named errors; this is enforcement.
    if let Some(iface) = op.abstract_interface.as_ref() {
        verify_op_interface(iface, kernel.program().as_ref(), context)?;
    }
    Ok(kernel)
}

/// SRD-108 Part B — verify a bound slot's interface against its
/// compiled op-template program:
///
/// - every `yields` name must exist on the program (a capture
///   extern slot or a declared output) with the declared type;
/// - every `needs` name the program declares as a slot must
///   carry the declared type (a need consumed only through
///   textual bind points declares no slot — the parent-chain
///   resolution type-checks those as usual).
///
/// Runs during pre-map synthesis — an interface violation fails
/// workload init, never a runtime critical section.
fn verify_op_interface(
    iface: &nmbrs_workload::model::OpInterface,
    program: &polydat::kernel::PolydatProgram,
    context: &str,
) -> Result<(), String> {
    let type_of = |name: &str| -> Option<polydat::ast::PortType> {
        program
            .input_port_type(name)
            .or_else(|| program.output_port_type(name))
    };
    // Yields: PRESENCE is the binder's load-time check (a capture
    // `as`-name); captures deliver through the wrapper stack's
    // wire writes, so a capture-only yield may hold no slot on
    // the op-template program itself. When a slot DOES
    // materialize (declared cast, binding reference), its type
    // must match the interface.
    for (name, declared) in &iface.yields {
        let Some(expected) = polydat::ast::PortType::from_keyword(declared) else {
            return Err(format!(
                "{context}: interface yield '{name}' declares unknown \
                 type '{declared}'"
            ));
        };
        if let Some(actual) = type_of(name)
            && actual != expected
        {
            return Err(format!(
                "{context}: interface yield '{name}' declares type \
                 {declared}, but the compiled op template carries \
                 {actual:?}"
            ));
        }
    }
    for (name, declared) in &iface.needs {
        let Some(expected) = polydat::ast::PortType::from_keyword(declared) else {
            return Err(format!(
                "{context}: interface need '{name}' declares unknown \
                 type '{declared}'"
            ));
        };
        if let Some(actual) = type_of(name)
            && actual != expected
        {
            return Err(format!(
                "{context}: interface need '{name}' declares type \
                 {declared}, but the compiled op template carries \
                 {actual:?}"
            ));
        }
    }
    // Results (SRD-109 Part 3): the projection wire's slot is
    // DECLARED from the interface type at op-template build (see
    // the result-binding declaration pass), so the check here is
    // keyword validity plus agreement when the slot materialized —
    // a mismatch means something else (a binding, a cast) claimed
    // the name at a different type.
    for (name, declared) in &iface.results {
        let Some(expected) = polydat::ast::PortType::from_keyword(declared) else {
            return Err(format!(
                "{context}: interface results wire '{name}' declares \
                 unknown type '{declared}'"
            ));
        };
        if let Some(actual) = type_of(name)
            && actual != expected
        {
            return Err(format!(
                "{context}: interface results wire '{name}' declares \
                 type {declared}, but the compiled op template carries \
                 {actual:?}"
            ));
        }
    }
    Ok(())
}

/// Internal binding name for a workload-declared metric's `value:`
/// expression. The metric's `value:` source is synthesised into the
/// op-template kernel as `<binding> := <expr>` so a single
/// closure-binding-economy walker handles slot allocation for
/// magic-extern references in both result-bindings AND
/// metric-value expressions. MetricsDispenser reads the same name
/// at cycle time via `ctx.wires.get` — the prefix keeps these
/// internal bindings from colliding with workload-declared output
/// names and lets diagnostic surfaces filter them out by prefix.
/// The `until:` expression from an op's `poll:` map, if it declared one.
///
/// Returns `None` for the row-count form (`mode` / `min_rows` / `max_rows`),
/// which stays supported — a workload that counts rows keeps working and a
/// workload that states a condition gets the condition.
fn template_poll_until(op: &nmbrs_workload::model::ParsedOp) -> Option<String> {
    op.params
        .get("poll")?
        .as_object()?
        .get("until")?
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn synthesize_metric_binding_name(metric_name: &str) -> String {
    format!("__metric_{metric_name}")
}

/// The kernel wire a metric's cell COORDINATE is lowered to, per
/// (metric, dimension).
///
/// Keyed by metric as well as dimension because two metrics in one scope may
/// place into the same dimension by different expressions — `bytes_out` from
/// one captured column and `bytes_in` from another. A per-dimension name would
/// collide and one placement would silently win.
///
/// Same `__` convention as [`synthesize_metric_binding_name`]: it cannot
/// collide with an author-declared output, and diagnostics can filter it as
/// internal.
pub fn synthesize_cell_binding_name(metric_name: &str, dimension: &str) -> String {
    format!("__cell_{metric_name}__{dimension}")
}

/// The phase-kernel wire an **inline objective expression** is lowered to
/// (SRD-86). `optimize.objective` is either a bare wire reference (read
/// directly off the phase kernel) or an inline expression;
/// [`synthesize_phase_scope_bindings`] lowers the latter to
/// `volatile __objective := <expr>` so the optimizer reads exactly one wire
/// regardless of which form the author wrote. The `__` prefix keeps it from
/// colliding with author-declared outputs (same convention as
/// [`synthesize_metric_binding_name`]).
pub const OBJECTIVE_WIRE: &str = "__objective";

/// Whether an `optimize.objective` value is a **bare wire reference** — a
/// single identifier read directly off the phase kernel — versus an **inline
/// expression** (operators, calls, dots, whitespace) that must be synthesized.
/// A bare reference is left untouched (no synthesis, current behavior); an
/// expression is lowered to [`OBJECTIVE_WIRE`].
pub fn objective_is_bare_wire(objective: &str) -> bool {
    let s = objective.trim();
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The phase-kernel wire the optimizer reads for `objective`: the bare name
/// itself, or the synthesized [`OBJECTIVE_WIRE`] for an inline expression.
/// Pairs with [`synthesize_phase_scope_bindings`], which emits the
/// `__objective` binding for the expression case — the two agree by sharing
/// [`objective_is_bare_wire`].
pub fn objective_wire(objective: &str) -> &str {
    if objective_is_bare_wire(objective) {
        objective.trim()
    } else {
        OBJECTIVE_WIRE
    }
}

/// Flatten a [`nmbrs_workload::model::ResultSpec`] into a single
/// Polydat source string suitable for
/// [`polydat::kernel::subcontext::SubcontextBuilder::add_result_bindings`].
/// String-shape entries pass through verbatim; map-shape entries
/// emit `<name> := <source>` lines (the same projection the
/// SRD-66 schema specifies); list-shape entries recurse.
///
/// Path-expression and built-in short forms (`count` / `ok`) in
/// map-shape entries land as bare Polydat expressions — `count` and
/// `ok` resolve to the magic-extern wires
/// [`SubcontextBuilder::add_result_bindings`] injects, while
/// path expressions like `rows[0].field` produce an unbound-
/// identifier compile error. The latter surfaces SRD-66's
/// "path expressions deferred until structural-body wire lands"
/// diagnostic.
fn collect_result_bindings_source(spec: &nmbrs_workload::model::ResultSpec) -> String {
    let mut out = String::new();
    spec.walk_fragments(|frag| match frag {
        nmbrs_workload::model::ResultFragment::Source(src) => {
            out.push_str(src);
            if !src.ends_with('\n') {
                out.push('\n');
            }
        }
        nmbrs_workload::model::ResultFragment::Named { name, source } => {
            // Path expressions (`rows[*].key`, `result[0].id`) are
            // dispenser-evaluated (SRD-70) — they are not polydat
            // source and would fail the result-bindings compile.
            // Only `count` / `ok` (magic-extern references) and
            // polydat-call entries compile into the kernel.
            let s = source.trim();
            if s == "count" || s == "ok" || s.contains('(') {
                out.push_str(&format!("{name} := {source}\n"));
            }
        }
    });
    out
}

// reason: cohesive scope builder — each argument is a distinct compile input
// (ops, iteration vars, outer manifest, workload params, …); grouping them
// into a struct would only relocate the same fields without adding clarity.
#[allow(clippy::too_many_arguments)]
pub fn build_scope(
    ops: &[ParsedOp],
    iteration_vars: &HashMap<String, String>,
    outer_manifest: &[crate::runner::ManifestEntry],
    workload_params: &HashMap<String, String>,
    phases: &HashMap<String, nmbrs_workload::model::WorkloadPhase>,
    phase_cycles: Option<&str>,
    exclude: &[String],
    // SRD-13f §"Wire-reference classification" — the parent
    // scope's compiled kernel. The synthesizer reads its
    // retained AST (for case 3 local-inclusion walks) and its
    // folded constant state (for case 1 promoted-final
    // emission). `None` for the workload-root build (it IS the
    // root; no parent).
    parent_kernel: Option<&polydat::kernel::PolydatKernel>,
) -> Result<BindingScope, String> {
    let mut scope = BindingScope::new();

    // --- Step 1: Classify op bindings by origin ---
    //
    // The first op's PolydatSource is the "base" (inherited/phase-level).
    // Subsequent ops: if their PolydatSource matches the base exactly,
    // they're inherited duplicates. If different, they're op-level
    // augmentations carrying new bindings.
    let mut base_source: Option<String> = None;

    for op in ops {
        if let BindingsDef::PolydatSource(src) = &op.bindings {
            let src = src.trim();
            if src.is_empty() {
                continue;
            }

            match &base_source {
                None => {
                    // First op's source becomes the base — classified as Inherited
                    base_source = Some(src.to_string());
                    scope.ingest_polydat_source(src, BindingOrigin::Inherited);
                }
                Some(base) => {
                    if src == base.as_str() {
                        // Identical to base — inherited duplicate, skip.
                        // validate() and emit() handle dedup.
                    } else {
                        // Different from base — this op has its own bindings.
                        // Ingest the DIFFERENCE (lines not in base) as Op origin,
                        // and the shared lines as Inherited. Compare by
                        // *logical* lines so multi-line expressions
                        // (function calls broken over several physical
                        // lines) stay intact instead of being split at
                        // the paren and ingested piecewise.
                        let base_logical: Vec<String> = logical_lines(base)
                            .into_iter()
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect();
                        for line in logical_lines(src) {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if base_logical.iter().any(|b| b == trimmed) {
                                // This line is inherited — already ingested from base
                            } else {
                                // This line is op-specific
                                scope.ingest_polydat_source(
                                    trimmed,
                                    BindingOrigin::Op(op.name.clone()),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // --- Step 2: Add iteration variables ---
    //
    // Iter vars (own + cascade-inherited) are part of the
    // **scope's contract** — they're names a phase or a child
    // scope is allowed to consume. Mark them required so the
    // compiler's DCE keeps the auto-passthrough output that
    // `extern <name>` produces. Without this, an iter var
    // referenced only from a deeper structure (e.g. a relevancy
    // `k:` field, or a downstream do-while condition) would be
    // pruned and pull-by-name would fail.
    for (var, val) in iteration_vars {
        scope.add_iteration_var(var, val);
        scope.add_required_output(var);
    }

    // --- Step 3: Generate auto-externs ---
    //
    // Names referenced in op templates but not defined in the scope
    // need extern declarations to wire to the outer kernel.
    let defined = scope.defined_names();
    let extern_names = scope.extern_names();

    // Collect all referenced names from op templates AND
    // binding source RHS. Names referenced in either need
    // extern declarations so the Polydat compile path can wire
    // them. Without scanning bindings, a phase whose binding
    // RHS uses `{outer_name}` (e.g. `dim := vector_dim(
    // "{dataset}:{profile}")`) wouldn't get extern declarations
    // for `dataset` and `profile`, leaving the Polydat string
    // interpolation desugar to fail at compile time.
    let mut referenced: HashSet<String> = HashSet::new();
    for op in ops {
        for value in op.op.values() {
            if let Some(s) = value.as_str() {
                for name in nmbrs_workload::bindpoints::referenced_bindings(s) {
                    referenced.insert(name);
                }
            }
        }
        if let Some(ref cond) = op.condition {
            let bare = cond
                .trim()
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or(cond.trim());
            referenced.insert(bare.to_string());
        }
        // `delay:` accepts the same shapes as `if:` — a bare
        // wire name (`delay: think_time`) or a `{...}` inline
        // expression. Both consume a binding and need to land
        // in `referenced` so the auto-extern + DCE-keepalive
        // passes provision them.
        if let Some(ref delay_spec) = op.delay {
            for raw in delay_spec.names() {
                let bare = raw
                    .trim()
                    .strip_prefix('{')
                    .and_then(|s| s.strip_suffix('}'))
                    .unwrap_or(raw.trim());
                referenced.insert(bare.to_string());
            }
        }
        // Bindings: scan Polydat source for `{name}` placeholders
        // that the Polydat string-interpolation desugar will treat
        // as wire references.
        if let BindingsDef::PolydatSource(src) = &op.bindings {
            collect_string_interp_refs(src, &mut referenced);
            // Also scan for bare identifiers used in binding
            // RHSs (e.g. `if(optimize_for == "LATENCY", …)`).
            // Without this, names like `optimize_for` flow
            // through to the Polydat compiler unresolved and get
            // auto-externed at the compiler's default type
            // (u64); `materialize_wiring_from_outer` then writes the
            // parent's Str value into the u64-typed slot, and
            // a `__u64_to_string` adapter inserted by type
            // dispatch panics at runtime on `as_u64()`. The
            // local-binding filter below mirrors the
            // build_op_template_scope_kernel cascade — names
            // declared by THIS scope's own bindings are
            // resolved locally, not externed.
            let body_locally_declared = scan_locally_declared_idents(src);
            for ident in scan_idents_in_polydat_source(src) {
                if !body_locally_declared.contains(&ident) {
                    referenced.insert(ident);
                }
            }
        }
        // Op params (`evaluations:`/`relevancy:`/`verify:`/...
        // hoisted by the workload parser) carry `{name}` bind-
        // point references too — `relevancy: { k: "{k}" }` is
        // a real consumer of the wire `k`, even though `k`
        // isn't on the op-template wire path. Without scanning
        // these, an iter var referenced *only* from param
        // config would fail to auto-extern from the parent
        // manifest, and runtime `pull(name)` calls for that
        // wire (e.g. `parse_count_param` on relevancy `k:`)
        // would panic with "unknown output variate".
        let mut param_refs: Vec<String> = Vec::new();
        crate::bindings::collect_param_bindings_into(&op.params, &[], &mut param_refs);
        for name in param_refs {
            referenced.insert(name);
        }
    }

    // Check for final shadowing violations
    for entry in outer_manifest {
        if entry.modifier == polydat::dsl::ast::BindingModifier::CONST
            && defined.contains(&entry.name)
        {
            return Err(format!(
                "cannot shadow 'final' binding '{}' from outer scope",
                entry.name
            ));
        }
    }

    // SRD-13f §"Wire-reference classification" — case 3 (local
    // matter inclusion). When the parent program is available
    // (AST mode), promote non-final referenced names from
    // cascade-via-extern (case 2) to inline-as-Inherited.
    //
    // Rules:
    // - `final` / `shared` outputs stay cascaded (case 2-like
    //   for now; promoted-final optimization is a follow-up).
    // - Cycle bindings / init bindings whose body lives in the
    //   parent's retained AST get pretty-printed and ingested as
    //   Inherited. Transitive dependencies (RHS Ident refs)
    //   walk the same rule via `local_inclusion_chain`.
    // - Names defined by `extern` ports in the parent stay
    //   cascaded.
    //
    // The auto-extern loop below then runs as-is and fills the
    // gap for names that didn't have an AST body to pull in.
    if let Some(parent_kernel_ref) = parent_kernel {
        let parent_prog = parent_kernel_ref.program();
        // Propagate the parent's coordinate input names into this
        // scope when it doesn't already have an `input ...: u64`
        // declaration of its own. Without this, an included
        // binding like `trip := testkit_throw_at(cycle, threshold, ...)`
        // references `cycle` but the auto-extern loop emits
        // `extern cycle: u64` (extern, not coord); set_inputs
        // propagation then skips it and per-cycle ticking dies.
        //
        // Coord names are just wire names the parent declared as
        // inputs; the child propagates them by declaring the
        // same `input ...: u64` line. Nothing special about any
        // specific name here.
        if scope.coordinates.is_none() {
            let coord_count = parent_prog.coord_count();
            if coord_count > 0 {
                let input_names = parent_prog.input_names();
                let coords: Vec<String> = input_names.into_iter().take(coord_count).collect();
                scope.coordinates = Some(format_input_decl_line(&coords).trim_end().to_string());
            }
        }

        // Names already accounted for: defined locally, declared
        // extern in subscope, or iter-vars. The inclusion-chain
        // walker uses this as its termination boundary so it
        // doesn't re-emit names the scope already satisfies.
        // (Coord input names like the one the workload author
        // declared via `input ...: u64` are not special — the
        // chain walker's `binding_ast_for` returns `None` for
        // coord inputs since they aren't binding statements, so
        // they self-terminate without explicit handling here.)
        let mut already_satisfied: HashSet<String> = HashSet::new();
        already_satisfied.extend(defined.iter().cloned());
        already_satisfied.extend(extern_names.iter().cloned());
        for var in iteration_vars.keys() {
            already_satisfied.insert(var.clone());
        }

        // Sort for determinism — HashSet iteration is randomised.
        // Clone names so the loop body can mutate `referenced`
        // (collecting transitive refs from included bindings).
        let mut refs_sorted: Vec<String> = referenced.iter().cloned().collect();
        refs_sorted.sort();
        for name in &refs_sorted {
            let name = name.as_str();
            if already_satisfied.contains(name) {
                continue;
            }
            let manifest_modifier = outer_manifest
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.modifier);
            let is_workload_param = workload_params.contains_key(name);
            if let Some(m) = manifest_modifier {
                use polydat::dsl::ast::BindingModifier;
                if m == BindingModifier::SHARED {
                    // SHARED stays in the cascade path — cells
                    // synchronise across kernels at runtime via
                    // SharedCell, not via const inlining.
                    continue;
                }
                if m == BindingModifier::CONST {
                    // SRD-13f §"Materialization gradient" — inline
                    // as `const NAME := <literal>` only when the
                    // upstream value is **statically known**. Per
                    // SRD-11 §"Two Evaluation Lifecycles", the
                    // `const` modifier has two implementation
                    // paths: compile-fold (the producing node is
                    // replaced with a leaf const node, value is
                    // part of the compiled artifact and identical
                    // across every activation) vs. scope-init pull
                    // (when the binding's wire chain touches
                    // iteration externs, the producing node stays
                    // as the original computation node; the value
                    // is computed per scope activation and lives
                    // only for that activation per SRD-13c
                    // §"const").
                    //
                    // The cascaded source built here gets compiled
                    // and cached on the phase scope-tree node, so
                    // its lifetime is the workload run — longer
                    // than any single activation. Inlining a
                    // scope-init-pulled value would freeze the
                    // first activation's value into the cached
                    // program forever, breaking every subsequent
                    // iteration of an enclosing comprehension
                    // (the symptom: `const mode := sm` inside a
                    // `for_each sm in alpha, beta` reads `alpha`
                    // for both iters).
                    //
                    // SRD-11 §"Provenance-Based Invalidation"
                    // gives the discriminator at zero cost: a
                    // truly statically-known node has empty input
                    // provenance (it depends on no graph inputs);
                    // a passthrough or computation node has bits
                    // set for the input slots it reads. We inline
                    // only when provenance is empty; otherwise we
                    // fall through to the auto-extern cascade
                    // below so each activation re-pulls the value
                    // through the chain via
                    // `materialize_wiring_from_outer` Step 3.
                    let statically_known = parent_kernel_ref
                        .program()
                        .output_index(name)
                        .map(|out_idx| {
                            let (node_idx, _) =
                                parent_kernel_ref.program().resolve_output_by_index(out_idx);
                            parent_kernel_ref
                                .program()
                                .input_provenance_for(node_idx)
                                .is_none_or(|p| p.is_zero())
                        })
                        .unwrap_or(true);
                    if statically_known
                        && let Some(value) = parent_kernel_ref.get_constant(name)
                        && let Some(natural) = value_to_param_string(value)
                    {
                        scope.add_param_binding(name, &natural);
                        already_satisfied.insert(name.to_string());
                        continue;
                    }
                    continue;
                }
            }
            // SRD-21 §"Where workload-param values live in the
            // kernel chain" — workload params are stable for the
            // run but live in the params-kernel as folded
            // constants, not in the workload-root program itself.
            // `parent_kernel.lookup(name)` reads through the
            // chain-wired input slot value, so this promotion
            // gives us const-folded workload-param values inside
            // every descendant scope's program — same
            // effective semantics as the pre-SRD-21 design
            // without poisoning the workload-root with `final`
            // constants that would block scenario-tree
            // shadowing.
            if is_workload_param
                && let Some(value) = parent_kernel_ref.lookup(name)
                && let Some(natural) = value_to_param_string(&value)
            {
                scope.add_param_binding(name, &natural);
                already_satisfied.insert(name.to_string());
                continue;
            }
            // Pull the inclusion chain. If the name isn't in the
            // parent's AST (or it's a final/shared binding in
            // the AST), the chain is empty — the auto-extern
            // loop below handles those.
            let chain = parent_prog.local_inclusion_chain(name, &already_satisfied);
            if chain.is_empty() {
                continue;
            }
            // Pretty-print each Statement in topological order
            // and ingest as Inherited. Track bound names so the
            // chain walker and the auto-extern loop see them
            // satisfied locally. Walk each binding's RHS to
            // collect transitive refs into `referenced` — these
            // are the names the included binding's expression
            // mentions but doesn't define (e.g. parent `final`
            // values like `dataset`); the auto-extern loop below
            // then emits externs for them.
            for stmt in chain {
                let line = polydat::dsl::pprint::pp_statement(stmt);
                scope.ingest_polydat_source(&line, BindingOrigin::Inherited);
                let body = match stmt {
                    polydat::dsl::ast::Statement::Binding(b) => Some(&b.value),
                    _ => None,
                };
                if let Some(expr) = body {
                    polydat::dsl::collect_expr_references(expr, &mut referenced);
                }
                if let polydat::dsl::ast::Statement::Binding(b) = stmt {
                    for t in &b.targets {
                        already_satisfied.insert(t.clone());
                    }
                }
            }
        }
    }

    // Refresh `defined` set after AST-mode inclusion may have
    // added Inherited bindings. The auto-extern loop below uses
    // this updated set to skip names now satisfied locally.
    let defined = scope.defined_names();

    // Generate extern declarations for referenced-but-undefined names
    for entry in outer_manifest {
        let is_iter_var = iteration_vars.contains_key(&entry.name);
        if referenced.contains(&entry.name)
            && !defined.contains(&entry.name)
            && !extern_names.contains(&entry.name)
            && !is_iter_var
        {
            let type_name = entry.port_type.to_keyword();
            scope.add_extern(&entry.name, type_name);
        }
    }

    // SRD-13f §"Wire-reference classification" — case 4:
    // unresolved → synthesizer-level validation error. Fires
    // only on descendant scopes (parent_kernel is Some). The
    // workload-root build skips this check; the Polydat compiler's
    // auto-input-inference path handles unresolved refs there.
    //
    // A reference is resolved if any of: defined locally,
    // declared extern (in subscope or auto-extern'd from outer
    // manifest), an iteration variable, a workload param key,
    // a coord name on this scope, or in the outer manifest at
    // all. Anything else is a typo or missing upstream binding.
    if parent_kernel.is_some() {
        let defined_now = scope.defined_names();
        let extern_now = scope.extern_names();
        let mut satisfied: HashSet<String> = HashSet::new();
        satisfied.extend(defined_now);
        satisfied.extend(extern_now);
        for var in iteration_vars.keys() {
            satisfied.insert(var.clone());
        }
        for name in workload_params.keys() {
            satisfied.insert(name.clone());
        }
        for entry in outer_manifest {
            satisfied.insert(entry.name.clone());
        }
        // Coord names from this scope's `input ...: u64` line.
        if let Some(coords_line) = &scope.coordinates
            && let Some(rhs) = coords_line.split(":=").nth(1)
        {
            let inner = rhs.trim().trim_start_matches('(').trim_end_matches(')');
            for n in inner.split(',') {
                let n = n.trim();
                if !n.is_empty() {
                    satisfied.insert(n.to_string());
                }
            }
        }
        // `referenced` is built from a textual scan that catches
        // both wire refs and bare identifiers — including GK
        // function names like `mod`, `hash`, `range` that
        // appear in binding RHS as call heads. Filter those out
        // via the registry lookup; only true wire references
        // should surface as unresolved.
        let mut unresolved: Vec<&String> = referenced
            .iter()
            .filter(|n| !satisfied.contains(n.as_str()))
            // Dotted names follow the field-access wire
            // convention (`q.cursor.idx` reads the wire
            // `q__cursor__idx`) — check the flattened spelling
            // the same way kernel lookup does.
            .filter(|n| !n.contains('.') || !satisfied.contains(n.replace('.', "__").as_str()))
            .filter(|n| !n.starts_with("__"))
            .filter(|n| polydat::dsl::registry::lookup(n).is_none())
            .collect();
        unresolved.sort();
        if !unresolved.is_empty() {
            let names: Vec<&str> = unresolved.iter().map(|s| s.as_str()).collect();
            let mut visible: Vec<&str> = satisfied.iter().map(|s| s.as_str()).collect();
            visible.sort();
            return Err(format!(
                "unresolved wire reference(s) {names:?}: not declared locally, \
                 not in parent manifest, not a workload param. \
                 Visible names in this scope: {visible:?}"
            ));
        }
    }

    // --- Step 4: Workload param cascade as extern slots ---
    //
    // Workload params are not the workload-root's authority — the
    // params-kernel sits one level above this scope and owns the
    // `const NAME := <literal>` declarations. The workload-root
    // program declares each param as `extern NAME: T`, and the
    // materialize-wiring-from-outer step at kernel construction
    // time copies the value out of the params-kernel's output
    // into this scope's input slot. Lookup for a param at the
    // workload-root then resolves via the input slot rather than
    // an own-folded constant — which is precisely what makes a
    // SetParam scope-tree node inserted between params-kernel
    // and workload-root able to shadow the value: its own
    // `const NAME := <override>` becomes the closer
    // materialize-source. If we baked `const NAME := <literal>`
    // here at the workload-root, that local constant would mask
    // the input slot via the get_constant fast-path in
    // PolydatKernel::lookup, and no scope-tree-level shadow could
    // get through.
    //
    // Author-declared `final` in the workload's `bindings:`
    // block is unaffected — that source goes through
    // workload_level_polydat ingestion in build_workload_root_kernel
    // and keeps whatever modifier the author wrote. Only the
    // auto-cascade of the workload `params:` block changes
    // shape.
    //
    // Phase-level build_scope callers pass an empty
    // `workload_params`; their workload params arrive via the
    // parent-scope kernel's manifest auto-extern pass.
    //
    // SRD-13f Push D: sort by name so program build order is
    // deterministic across processes. HashMap iteration is
    // randomized per process (Rust default hasher).
    let defined = scope.defined_names(); // refresh after externs
    let mut params_sorted: Vec<(&String, &String)> = workload_params.iter().collect();
    params_sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (name, value) in params_sorted {
        if defined.contains(name) {
            // Author already declared `name` in the workload's
            // own bindings block. Honor the author's declaration
            // (it carries whatever modifier they wrote) and skip
            // the auto-extern — declaring an extern with the
            // same name as a local binding would be a compile
            // error.
            continue;
        }
        let type_name = workload_param_type_name(value);
        scope.add_extern(name, type_name);
        // Mark as required so DCE keeps the auto-passthrough
        // output. Descendant scopes auto-extern the param via
        // the workload-root manifest; if the workload-root's
        // own program prunes the param away, that cascade
        // breaks.
        scope.add_required_output(name);
    }
    // Also check phase config values for param refs
    for phase in phases.values() {
        if let Some(ref c) = phase.cycles
            && c.starts_with('{')
            && c.ends_with('}')
        {
            let name = &c[1..c.len() - 1];
            if workload_params.contains_key(name) && !scope.defined_names().contains(name) {
                scope.add_param_binding(name, &workload_params[name]);
            }
        }
    }

    // --- Step 5: Inline expression extraction ---
    //
    // {{expr}} in op templates becomes __expr_N bindings.
    // (Note: the op template rewriting happens separately in the caller
    // since it mutates op fields, not the scope.)
    let mut inline_idx = 0usize;
    let mut expr_to_name: HashMap<String, String> = HashMap::new();
    let mut collect = |s: &str| {
        for bp in nmbrs_workload::bindpoints::extract_bind_points(s) {
            if let nmbrs_workload::bindpoints::BindPoint::InlineDefinition(ref expr) = bp
                && !expr_to_name.contains_key(expr)
            {
                let name = format!("__expr_{inline_idx}");
                inline_idx += 1;
                expr_to_name.insert(expr.clone(), name);
            }
        }
    };
    for op in ops {
        for value in op.op.values() {
            if let Some(s) = value.as_str() {
                collect(s);
            }
        }
        // Inline expressions in `if:` and `delay:` count too —
        // those get hoisted out of `op.op` by the parser into
        // dedicated fields on `ParsedOp`.
        if let Some(s) = &op.condition {
            collect(s);
        }
        if let Some(spec) = &op.delay {
            for name in spec.names() {
                collect(name);
            }
        }
    }
    for (expr, name) in &expr_to_name {
        scope.add_inline_expr(name, expr);
    }

    // --- Step 6: Collect required outputs ---
    for op in ops {
        for value in op.op.values() {
            if let Some(s) = value.as_str() {
                for name in nmbrs_workload::bindpoints::referenced_bindings(s) {
                    if !exclude.contains(&name) {
                        scope.add_required_output(&name);
                    }
                }
            }
        }
        // Required-output collection for `if:` and `delay:`.
        //
        // The condition / delay value may be one of:
        //   1. `{{expr}}` — inline expression. Step 5 above
        //      synthesised a `__expr_N := expr` binding; the
        //      required output is that synthesised name.
        //   2. `{name}` — a single-brace binding reference.
        //   3. A bare identifier — legacy "name a binding"
        //      form, no braces.
        // The previous strip-one-pair-of-braces logic only
        // handled forms 2 and 3; for `{{expr}}` it produced a
        // half-stripped string `{expr}` that didn't match any
        // binding and let DCE drop the synthesised
        // `__expr_N`. Walk through `extract_bind_points` so
        // every form resolves to the right output name.
        let mut collect_required = |s: &str| {
            let trimmed = s.trim();
            // Bracketed forms: `{{expr}}`, `{:=expr:=}`,
            // `{name}`, `{expr-with-operators}`.
            let bps = nmbrs_workload::bindpoints::extract_bind_points(trimmed);
            if !bps.is_empty() {
                for bp in bps {
                    match bp {
                        nmbrs_workload::bindpoints::BindPoint::InlineDefinition(expr) => {
                            if let Some(name) = expr_to_name.get(&expr)
                                && !exclude.contains(name)
                            {
                                scope.add_required_output(name);
                            }
                        }
                        nmbrs_workload::bindpoints::BindPoint::Reference { name, .. } => {
                            if !exclude.contains(&name) {
                                scope.add_required_output(&name);
                            }
                        }
                    }
                }
                return;
            }
            // Bare-identifier form (no braces) — legacy.
            if !trimmed.is_empty() && !exclude.contains(&trimmed.to_string()) {
                scope.add_required_output(trimmed);
            }
        };
        if let Some(ref cond) = op.condition {
            collect_required(cond);
        }
        if let Some(ref delay_spec) = op.delay {
            for name in delay_spec.names() {
                collect_required(name);
            }
        }
        // SRD-40b §6: synthetic-metric `value:` references must
        // survive DCE so the dispenser's Polydat pull plan can resolve
        // them. Bare-name values (the SRD-40b §1 canonical form)
        // refer to a wire produced somewhere in scope; non-bare
        // expressions are deferred to Phase 9 elsewhere — for the
        // bare-name case we mark the wire required.
        for spec in op.metrics.values() {
            let trimmed = spec.value.trim();
            let bare =
                !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_');
            if bare && !exclude.contains(&trimmed.to_string()) {
                scope.add_required_output(trimmed);
            }
        }
        // SRD-40b §5 result-as-GK: each `result:` wire reads a
        // path expression off the response body and exposes it as
        // a Polydat wire. The wire's *name* is what subsequent
        // wrappers (metrics, validation) pull against — mark each
        // declared result wire as required so the kernel exposes
        // an extern slot for it on the post-execute write path.
        // SRD-66: result-wire names are already declared as
        // wires through the op's `bindings:` block (when the
        // workload uses `extern X: T` for shared-cell writes)
        // or are independent of the op-template kernel (when
        // the wire is consumed only by the result dispenser's
        // capture map). Marking them as required outputs here
        // would double-declare the `__port_*` for any name
        // that's also an extern, so the kernel-synthesis-side
        // wiring is left to Push 2's full kernel-driven path
        // (the SRD-66 §"Compilation lifecycle" closure-binding
        // rule). This branch intentionally does nothing for now.
        let _ = op.result.as_ref();
        crate::bindings::collect_param_bindings_into(
            &op.params,
            exclude,
            &mut scope.required_outputs,
        );
    }

    // Config refs (from cycles={expr})
    if let Some(cycles_spec) = phase_cycles
        && cycles_spec.starts_with('{')
        && cycles_spec.ends_with('}')
    {
        let mut inner = cycles_spec[1..cycles_spec.len() - 1].to_string();
        for (v, val) in iteration_vars {
            inner = inner.replace(&format!("{{{v}}}"), val);
        }
        inner = crate::runner::expand_workload_params(&inner, workload_params);
        scope.add_config_ref(&inner);
    }

    Ok(scope)
}

/// Apply iteration-variable substitution to *op-template
/// strings only* — `raw:`, `prepared:`, `stmt:`, etc.
///
/// Iter vars flow into Polydat binding source as wires (declared as
/// externs by `BindingScope::add_iteration_var` and bound at
/// runtime via the standard input mechanism), so this helper
/// **does not** touch `op.bindings`. It only rewrites the
/// op-template field values, where `{var}` placeholders refer
/// to structural elements (table names, keyspace names,
/// optimize-for hints) that adapters need as literal text —
/// not as bind variables. CQL's `prepared:` form, for example,
/// converts every remaining `{name}` in the statement to a `?`
/// bind marker; iter vars in structural positions (`INSERT
/// INTO ks.{table} ...`) must be substituted away before that
/// conversion runs, since CQL doesn't permit `?` for table
/// names.
/// SRD-68 Push 5c — validate-only walk. Same semantic as
/// [`resolve_placeholders_via_kernel`] but DOES NOT mutate the op
/// strings. Walks every `{name}` placeholder in the ops' op fields
/// and op-level params, accumulating diagnostics for unresolved
/// references; returns `Result<(), String>` describing any
/// unresolved bindpoints.
///
/// Used at phase activation as the single workload-load-time
/// validation step. Adapters now do their own cycle-time
/// resolution (CQL: construction-time structural via
/// `canonical_kernel.lookup` + cycle-time per-cycle via
/// `WireSource::get`; non-CQL: cycle-time via
/// `synthesis::resolve_cached →
/// substitute_bind_points_with_state` against `main_kernel`).
/// Mutation of the workload model is no longer load-bearing —
/// only the diagnostic surface is.
pub fn validate_placeholders_via_kernel(
    ops: &[ParsedOp],
    kernel: &polydat::kernel::PolydatKernel,
) -> Result<(), String> {
    let per_cycle_names = collect_phase_binding_lhs_names(ops);

    let mut errors: Vec<String> = Vec::new();
    let in_scope = || -> Vec<String> {
        let prog = kernel.program();
        let mut names: Vec<String> = prog.output_names().iter().map(|s| s.to_string()).collect();
        for n in prog.input_names() {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names.sort();
        names
    };
    for op in ops.iter() {
        let op_name = op.name.clone();
        for (key, value) in op.op.iter() {
            let path = format!("op '{op_name}' field '{key}'");
            // Clone-then-discard: `resolve_placeholders_in_json`
            // requires `&mut serde_json::Value` but we don't
            // care about its mutations — only the `errors` it
            // accumulates. The clone is shallow per JSON value
            // and runs once per field at workload-load time.
            let mut throwaway = value.clone();
            resolve_placeholders_in_json(
                &mut throwaway,
                kernel,
                &per_cycle_names,
                &path,
                &mut errors,
            );
        }
        for (key, value) in op.params.iter() {
            if key == "gutter" {
                continue; // runtime-resolved templates (see the mutable pass below)
            }
            let path = format!("op '{op_name}' param '{key}'");
            let mut throwaway = value.clone();
            resolve_placeholders_in_json(
                &mut throwaway,
                kernel,
                &per_cycle_names,
                &path,
                &mut errors,
            );
        }
    }

    if errors.is_empty() {
        return Ok(());
    }
    let in_scope_str = in_scope().join(", ");
    let mut out =
        String::from("placeholder resolution failed (single read path: Polydat Kernel lookup):\n");
    for e in &errors {
        out.push_str("  - ");
        out.push_str(e);
        out.push('\n');
    }
    out.push_str(&format!(
        "  in-scope names at this kernel: [{in_scope_str}]"
    ));
    Err(out)
}

/// SRD-68 Push 5c — resolve `{name}` placeholders in a single
/// op's `params` against `kernel`. Used by validation wrappers
/// at construction time to pre-resolve config like `relevancy.k`
/// = `"{k}"` against their dispenser's canonical kernel, so the
/// downstream spec parsers see a literal value (`10`) rather
/// than a surviving placeholder string.
///
/// Per-cycle binding LHS names pass through unchanged — the
/// wrapper resolves those via wires at cycle time (e.g.
/// `relevancy.expected = "{ground_truth}"` stays as-is so the
/// wrapper can register it on the fixture's pull plan and
/// read it per cycle).
///
/// Single-op variant of the legacy bulk-mutation pass; the
/// per-template granularity lets each wrapper resolve against
/// its own dispenser's canonical kernel rather than a shared
/// activity-layer parent.
pub fn resolve_placeholders_in_op_params(
    op: &mut ParsedOp,
    kernel: &polydat::kernel::PolydatKernel,
) -> Result<(), String> {
    let per_cycle_names = collect_phase_binding_lhs_names(std::slice::from_ref(op));

    let mut errors: Vec<String> = Vec::new();
    let in_scope = || -> Vec<String> {
        let prog = kernel.program();
        let mut names: Vec<String> = prog.output_names().iter().map(|s| s.to_string()).collect();
        for n in prog.input_names() {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names.sort();
        names
    };
    let op_name = op.name.clone();
    for (key, value) in op.params.iter_mut() {
        // `gutter:` templates resolve at RUNTIME — during-forms per op
        // completion on the fiber wires, `final:` at phase end with a
        // status-metric fallback (`{recall}`, `{latency_p50}`) that has
        // no kernel presence here. Unresolved names degrade to visible
        // literal text in the cell, so the strict pre-resolution pass
        // must not reject them.
        if key == "gutter" {
            continue;
        }
        let path = format!("op '{op_name}' param '{key}'");
        resolve_placeholders_in_json(value, kernel, &per_cycle_names, &path, &mut errors);
    }
    if errors.is_empty() {
        return Ok(());
    }
    let in_scope_str = in_scope().join(", ");
    let mut out = String::from("param-placeholder resolution failed:\n");
    for e in &errors {
        out.push_str("  - ");
        out.push_str(e);
        out.push('\n');
    }
    out.push_str(&format!(
        "  in-scope names at this kernel: [{in_scope_str}]"
    ));
    Err(out)
}

// SRD-68 Push 5 cleanup: the legacy
// `resolve_placeholders_via_kernel` (op-field text mutation) and
// `resolve_placeholders_in_params_only` (bulk op-params mutation)
// are both retired. The executor calls
// [`validate_placeholders_via_kernel`] (pure walker) at workload
// load to surface unresolved-bindpoint diagnostics; adapters
// resolve op-field placeholders themselves at construction (CQL
// prepared via `resolve_structural_and_mark_remaining`) or at
// cycle time (CQL raw via `substitute_via_wires`); validation
// wrappers resolve their own op.params at construction via
// [`resolve_placeholders_in_op_params`] against the dispenser's
// own canonical kernel. The workload model is no longer mutated
// — `OpDispenser::describe()` returns pristine yaml.

/// Scan `ops`' `bindings:` text for the LHS names that get
/// produced as per-cycle wires. The scan is intentionally
/// liberal (LHS can be `cursor q = …`, `init x = …`, `name :=
/// expr`, `extern n: type`, `const n := expr`, `shared n := expr`,
/// destructured `(a, b) := …`). Anything that survives the LHS
/// strip becomes a known name; the substitution path uses this
/// set to distinguish "per-cycle wire — leave for the dispenser"
/// from "typo or missing cascade — error."
fn collect_phase_binding_lhs_names(ops: &[ParsedOp]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for op in ops {
        if let BindingsDef::PolydatSource(src) = &op.bindings {
            for line in logical_lines(src) {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                // `input` declarations declare per-cycle wire names
                // by definition — the runtime sets them per
                // iteration. Both surface forms are handled:
                //   input cycle: u64
                //   input (cycle: u64, q: f64)
                // Without this, `{cycle}` in op templates would
                // resolve at compile time against the kernel's
                // initial value (0) instead of being deferred to
                // per-iteration substitution.
                if let Some(rest) = trimmed.strip_prefix("input ") {
                    let rest = rest.trim();
                    if let Some(inner) = rest.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
                        for piece in inner.split(',') {
                            let n = piece.trim().split(':').next().unwrap_or("").trim();
                            if is_bare_ident(n) && !out.contains(&n.to_string()) {
                                out.push(n.to_string());
                            }
                        }
                    } else {
                        let n = rest.split(':').next().unwrap_or("").trim();
                        if is_bare_ident(n) && !out.contains(&n.to_string()) {
                            out.push(n.to_string());
                        }
                    }
                    continue;
                }
                let lhs_end = trimmed
                    .find(":=")
                    .or_else(|| trimmed.find('='))
                    .unwrap_or(trimmed.len());
                let mut lhs = &trimmed[..lhs_end];
                // Strip ALL leading wire-coloring modifiers in
                // any order. `volatile final` and `final shared`
                // both need their bare name extracted, and any
                // future modifier added to the SRD-10 set must
                // appear here too.
                loop {
                    let mut matched = false;
                    for prefix in [
                        "cursor ",
                        "init ",
                        "extern ",
                        "const ",
                        "final ",
                        "shared ",
                        "volatile ",
                        "private ",
                    ] {
                        if let Some(stripped) = lhs.strip_prefix(prefix) {
                            lhs = stripped.trim();
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        break;
                    }
                }
                let lhs = lhs.trim();
                // Destructured tuple LHS: (a, b, c) := ...
                if let Some(inner) = lhs.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
                    for piece in inner.split(',') {
                        let n = piece.trim().trim_end_matches(':').trim();
                        if is_bare_ident(n) && !out.contains(&n.to_string()) {
                            out.push(n.to_string());
                        }
                    }
                    continue;
                }
                // Type-annotated single LHS: `name: type`.
                let bare = lhs.split(':').next().unwrap_or(lhs).trim();
                if is_bare_ident(bare) && !out.contains(&bare.to_string()) {
                    out.push(bare.to_string());
                }
            }
        }
    }
    out
}

fn is_bare_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True iff `s` is a polydat string literal — opens with `"`,
/// closes with `"`, and (conservatively) contains no
/// unescaped closing quote in the middle. This is the shape
/// `format_jval_as_polydat_literal` produces for YAML strings
/// (`"hello"`) so the param-binding classifier can pass it
/// through without re-quoting.
fn is_polydat_quoted_string(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    if !s.starts_with('"') || !s.ends_with('"') {
        return false;
    }
    // Walk the inner span; reject if we find an unescaped `"`
    // before the final character (which would mean the outer
    // quotes don't actually pair).
    let bytes = s.as_bytes();
    let mut i = 1;
    let last = bytes.len() - 1;
    while i < last {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            return false;
        }
        i += 1;
    }
    true
}

/// True iff `s` looks like a polydat array literal: `[` …
/// matched brackets … `]`. Conservative — we don't validate
/// the elements here; the polydat parser handles that when
/// the `const X := [...]` line compiles. The check just
/// distinguishes "this is already polydat array syntax" from
/// "this is a raw string that happens to contain brackets".
fn is_polydat_array_literal(s: &str) -> bool {
    if !s.starts_with('[') || !s.ends_with(']') {
        return false;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for c in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod polydat_param_classifier_tests {
    use super::*;

    #[test]
    fn scan_idents_skips_hash_comments() {
        // `#` is a YAML-style line comment in Polydat (dsl/lexer.rs).
        // The ident scanner must skip it too, or comment words leak in
        // as phantom wire references (regression: finalize_index's
        // bindings comments).
        let src = "# Forwarding bindings: allow-list = bindings + params\n\
                   pct := mul(active, 2)  # trailing comment with words\n";
        let idents = scan_idents_in_polydat_source(src);
        assert!(
            idents.contains("active"),
            "real ident must be found: {idents:?}"
        );
        for word in [
            "Forwarding",
            "bindings",
            "allow",
            "list",
            "params",
            "trailing",
            "comment",
            "words",
        ] {
            assert!(
                !idents.contains(word),
                "comment word '{word}' must not be scanned as a wire ref: {idents:?}"
            );
        }
    }

    #[test]
    fn scan_idents_skips_block_conditional_keywords() {
        // Block-form conditional selection puts bare `if` / `else` in expression
        // position. This scanner is textual, so without the keyword entries they were
        // emitted as wire references and the phase failed with
        // `unresolved wire reference(s) ["else", "if"]` (regression: finalize_index's
        // seg_mib_mean binding).
        let src = "seg_mib_mean := if segments > 0 { total / max(segments, 1) } else { 0 }\n";
        let idents = scan_idents_in_polydat_source(src);
        assert!(
            idents.contains("segments"),
            "real wire must still be found: {idents:?}"
        );
        assert!(
            idents.contains("total"),
            "real wire must still be found: {idents:?}"
        );
        for kw in ["if", "else"] {
            assert!(
                !idents.contains(kw),
                "conditional keyword '{kw}' must not scan as a wire ref: {idents:?}"
            );
        }
    }

    #[test]
    fn scan_locally_declared_skips_hash_comment_lines() {
        let src = "# total = sum of parts\n\
                   shared sstables := 0\n";
        let decls = scan_locally_declared_idents(src);
        assert!(decls.contains("sstables"));
        // `# total = ...` has an `=`; it must not be parsed as a decl.
        assert!(
            !decls.contains("total"),
            "comment LHS must not declare: {decls:?}"
        );
    }

    #[test]
    fn bare_identifier_classifier_accepts_idents_and_rejects_other_shapes() {
        assert!(is_bare_ident("sm"));
        assert!(is_bare_ident("source_model"));
        assert!(is_bare_ident("k_values"));
        assert!(is_bare_ident("_underscore"));
        assert!(is_bare_ident("a1"));
        // Rejects strings that don't fit the ident grammar.
        assert!(!is_bare_ident(""));
        assert!(!is_bare_ident("1abc"), "ident can't start with digit");
        assert!(!is_bare_ident("foo bar"), "no spaces");
        assert!(!is_bare_ident("[a, b]"));
        assert!(!is_bare_ident("\"quoted\""));
        assert!(!is_bare_ident("foo+bar"));
        // Reserved-ish keywords stay valid as identifiers here;
        // the polydat parser is the authority on what's
        // reserved.
        assert!(is_bare_ident("true"));
        assert!(is_bare_ident("false"));
    }

    #[test]
    fn polydat_quoted_string_accepts_paired_quotes_only() {
        assert!(is_polydat_quoted_string("\"hello\""));
        assert!(is_polydat_quoted_string("\"\""));
        assert!(is_polydat_quoted_string("\"with \\\"escaped\\\" inner\""));
        // Rejects shapes that aren't paired-quote.
        assert!(!is_polydat_quoted_string("hello"));
        assert!(!is_polydat_quoted_string("\"open-only"));
        assert!(!is_polydat_quoted_string("close-only\""));
        assert!(!is_polydat_quoted_string(""));
        assert!(!is_polydat_quoted_string("\""));
    }

    #[test]
    fn polydat_array_literal_balances_brackets() {
        assert!(is_polydat_array_literal("[1, 2, 3]"));
        assert!(is_polydat_array_literal("[]"));
        assert!(
            is_polydat_array_literal("[[1, 2], [3, 4]]"),
            "nested arrays balance"
        );
        assert!(is_polydat_array_literal("[\"a\", \"b\"]"));
        // Strings containing `]` don't break the count.
        assert!(is_polydat_array_literal("[\"a]b\", \"c\"]"));
        // Rejects unbalanced shapes.
        assert!(!is_polydat_array_literal("[1, 2"));
        assert!(!is_polydat_array_literal("1, 2]"));
        // `[1][2]` passes the conservative shape check (depth
        // returns to zero) — polydat's parser rejects it
        // downstream as invalid syntax. The classifier is a
        // routing heuristic, not a validator.
    }
}

/// Recursively walk a JSON value and resolve every `{name}`
/// placeholder via [`PolydatKernel::lookup`]. Non-resolving names
/// that are in the per-cycle binding set stay as-is (the
/// dispenser will resolve them at execute time); anything else
/// gets pushed onto `errors`.
///
/// Object keys are *not* rewritten — keys are the closed-vocab
/// field name surface, distinct from the value-bearing
/// placeholders.
fn resolve_placeholders_in_json(
    value: &mut serde_json::Value,
    kernel: &polydat::kernel::PolydatKernel,
    per_cycle_names: &[String],
    field_path: &str,
    errors: &mut Vec<String>,
) {
    match value {
        serde_json::Value::String(s) => {
            match resolve_placeholders_in_string(s, kernel, per_cycle_names, field_path) {
                Ok(out) => *value = serde_json::Value::String(out),
                Err(es) => errors.extend(es),
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter_mut().enumerate() {
                let p = format!("{field_path}[{i}]");
                resolve_placeholders_in_json(v, kernel, per_cycle_names, &p, errors);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let p = format!("{field_path}.{k}");
                resolve_placeholders_in_json(v, kernel, per_cycle_names, &p, errors);
            }
        }
        _ => {}
    }
}

/// Walk one string, replace each `{name}` placeholder with the
/// kernel's `lookup` result for `name`. Bare-ident-only — the
/// qualified `{bind:…}` / `{capture:…}` / `{input:…}` /
/// `{param:…}` shapes and the inline `{{expr}}` shape pass
/// through untouched (consumed downstream by the dispenser /
/// inline-expression desugar). Names that fall through to the
/// per-cycle binding set also pass through. Anything else is
/// returned as an error in the `Err` Vec, with the field path
/// for context.
fn resolve_placeholders_in_string(
    s: &str,
    kernel: &polydat::kernel::PolydatKernel,
    per_cycle_names: &[String],
    field_path: &str,
) -> Result<String, Vec<String>> {
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut errors: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n {
        // `\{` and `\}` are escapes — passthrough one char.
        if bytes[i] == b'\\' && i + 1 < n && (bytes[i + 1] == b'{' || bytes[i + 1] == b'}') {
            out.push(bytes[i] as char);
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        // `{{ ... }}` is an inline expression — passthrough.
        if i + 1 < n && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the matching `}}`.
            let start = i;
            let mut j = i + 2;
            while j + 1 < n && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            let end = (j + 2).min(n);
            out.push_str(&s[start..end]);
            i = end;
            continue;
        }
        if bytes[i] != b'{' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Find the matching `}` for this `{`.
        let body_start = i + 1;
        let mut j = body_start;
        while j < n && bytes[j] != b'}' {
            j += 1;
        }
        if j >= n {
            // Unterminated `{` — treat as literal char and move on.
            out.push('{');
            i += 1;
            continue;
        }
        let body = &s[body_start..j];
        let after = j + 1;

        // Qualified references stay as-is for downstream.
        if body.contains(':') {
            out.push('{');
            out.push_str(body);
            out.push('}');
            i = after;
            continue;
        }

        // Empty body — leave as-is, validator catches.
        if body.is_empty() {
            out.push('{');
            out.push_str(body);
            out.push('}');
            i = after;
            continue;
        }

        // Not a bare identifier — pass through (could be a format spec).
        if !is_bare_ident(body) {
            out.push('{');
            out.push_str(body);
            out.push('}');
            i = after;
            continue;
        }

        // Names that are per-cycle (coordinates declared via
        // `input (cycle: u64, ...: u64)`, or LHS of phase bindings)
        // MUST be deferred to per-cycle resolution. Their value
        // varies per iteration; pre-resolving against the parent
        // kernel here would bake in iteration 0's value (0) and
        // every subsequent iteration would emit the same string.
        if per_cycle_names.iter().any(|n| n == body) {
            out.push('{');
            out.push_str(body);
            out.push('}');
            i = after;
            continue;
        }
        // Bare ident — try kernel lookup. Computed outputs
        // (node-backed) aren't found by `lookup` (it only reads
        // input slots + constants), but they ARE valid wires
        // visible at this scope — accept them by checking
        // `resolve_output`. Per-cycle resolution at dispenser
        // time handles the actual pull.
        match kernel.lookup(body) {
            Some(v) => out.push_str(&v.to_display_string()),
            None if kernel.program().resolve_output(body).is_some() => {
                // Defer to per-cycle resolution. Emit the
                // placeholder unchanged.
                out.push('{');
                out.push_str(body);
                out.push('}');
            }
            None => {
                errors.push(format!(
                    "{field_path}: '{{{body}}}' did not resolve in scope and is \
                     not a per-cycle binding declared by this phase"
                ));
                // Still push the placeholder as-is so the rest of
                // the string remains parseable for further error
                // collection on the same field.
                out.push('{');
                out.push_str(body);
                out.push('}');
            }
        }
        i = after;
    }

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// Rewrite inline expressions (`{{expr}}`) in op template strings to
/// use named binding references (`{__expr_N}`).
///
/// Returns the expression-to-name map for the caller to know what
/// was rewritten.
pub fn rewrite_inline_exprs(ops: &mut [ParsedOp]) -> HashMap<String, String> {
    // SRD-13d: each op template is its own Polydat scope. Inline
    // `{{<expr>}}` rewrites are Polydat matter that belongs to the
    // op-template scope, not to the shared phase scope. So each
    // op gets its OWN expression-to-name mapping, with
    // op-locally unique synth names — no cross-op dedup. Two ops
    // with textually identical inline expressions
    // (`if: cql_dialect == 'cndb'` on both `indexes_present_cndb`
    // and `indexes_built_cndb`) now get distinct
    // `__expr_N`/`__expr_M` names. Without this, both ops
    // injected the same `__expr_1 := cql_dialect == 'cndb'`
    // line into their bindings; the phase-scope ingest then saw
    // two ops each declaring `__expr_1` and tripped the
    // ride-along-uniqueness check (SRD-13c §"Op overriding op
    // shadow").
    //
    // The global `inline_idx` counter still increments
    // monotonically so synth names are workload-wide unique;
    // the per-op MAP keeps within-op dedup (same expression in
    // multiple fields of one op — `if: x == 1` and
    // `metric: x == 1` — collapses to a single `__expr_N` for
    // that op).
    let mut inline_idx = 0usize;
    let mut per_op_expr_to_name: Vec<HashMap<String, String>> =
        (0..ops.len()).map(|_| HashMap::new()).collect();
    let collect_from = |s: &str, idx: &mut usize, op_map: &mut HashMap<String, String>| {
        for bp in nmbrs_workload::bindpoints::extract_bind_points(s) {
            if let nmbrs_workload::bindpoints::BindPoint::InlineDefinition(ref expr) = bp {
                op_map.entry(expr.clone()).or_insert_with(|| {
                    let n = format!("__expr_{idx}");
                    *idx += 1;
                    n
                });
            }
        }
    };
    for (op_index, op) in ops.iter().enumerate() {
        let op_map = &mut per_op_expr_to_name[op_index];
        for value in op.op.values() {
            if let Some(s) = value.as_str() {
                collect_from(s, &mut inline_idx, op_map);
            }
        }
        if let Some(s) = &op.condition {
            collect_from(s, &mut inline_idx, op_map);
        }
        if let Some(spec) = &op.delay {
            for name in spec.names() {
                collect_from(name, &mut inline_idx, op_map);
            }
        }
    }
    // For diagnostics + downstream: the legacy single
    // `expr_to_name` return value flattens the per-op maps.
    // Names are unique across ops because the inline_idx
    // counter is global, so the union is collision-free.
    let expr_to_name: HashMap<String, String> = per_op_expr_to_name
        .iter()
        .flat_map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())))
        .collect();

    // Inject the synthesised `__expr_N := expr` bindings into
    // the first op's Polydat bindings source so `build_scope`'s
    // normal ingestion pass picks them up. Without this the
    // op fields get rewritten to reference `{__expr_N}` but
    // no binding declaring `__expr_N` ever lands in the
    // scope, so compilation fails with "unresolved bind
    // point '{__expr_N}'". Callers used to discard the
    // returned `expr_to_name` mapping and trust some other
    // path to install the bindings — there isn't one. Doing
    // the injection here keeps `rewrite_inline_exprs`
    // self-contained: every output rewrite has a matching
    // binding emitted.
    // Inject + rewrite per op. Each op uses ONLY its own
    // expr_to_name mapping (per_op_expr_to_name[op_index]).
    // Synth lines land in that op's bindings; field rewrites
    // see only that op's expression names. SRD-13d: the
    // op-template scope owns its own Polydat matter, including
    // synth bindings from inline expressions.
    if expr_to_name.is_empty() {
        return expr_to_name;
    }
    use nmbrs_workload::model::BindingsDef;
    for (op_index, op) in ops.iter_mut().enumerate() {
        let op_map = &per_op_expr_to_name[op_index];
        if op_map.is_empty() {
            continue;
        }

        // Build synth lines for this op, deterministically
        // ordered by synth name.
        let mut entries: Vec<(&String, &String)> = op_map.iter().collect();
        entries.sort_by(|a, b| a.1.cmp(b.1));
        let mut synth_lines = String::new();
        for (expr, name) in &entries {
            synth_lines.push_str(&format!("\n{name} := {expr}"));
        }

        // Inject into op.bindings. Map → PolydatSource conversion
        // mirrors the existing scope-source assembly path.
        match &mut op.bindings {
            BindingsDef::PolydatSource(s) => {
                if s.trim().is_empty() {
                    *s = synth_lines.trim_start_matches('\n').to_string();
                } else {
                    s.push_str(&synth_lines);
                }
            }
            BindingsDef::Map(_) => {
                if let BindingsDef::Map(map) = &op.bindings {
                    let mut existing = String::new();
                    for (k, v) in map.iter() {
                        existing.push_str(&format!("{k} := {v}\n"));
                    }
                    op.bindings = BindingsDef::PolydatSource(format!("{existing}{synth_lines}"));
                }
            }
        }

        // Rewrite this op's fields using its own mapping only.
        let rewrite = |s: &str| -> String {
            let mut rewritten = s.to_string();
            for (expr, name) in op_map {
                rewritten = rewritten.replace(&format!("{{{{{expr}}}}}"), &format!("{{{name}}}"));
                rewritten = rewritten.replace(&format!("{{:={expr}:=}}"), &format!("{{{name}}}"));
                rewritten = rewritten.replace(&format!("{{:={expr}}}"), &format!("{{{name}}}"));
                rewritten = rewritten.replace(&format!("{{{expr}}}"), &format!("{{{name}}}"));
            }
            rewritten
        };
        for value in op.op.values_mut() {
            if let Some(s) = value.as_str() {
                *value = serde_json::Value::String(rewrite(s));
            }
        }
        if let Some(s) = &op.condition {
            op.condition = Some(rewrite(s));
        }
        if let Some(spec) = &op.delay {
            // Rewrite each binding name in place, preserving
            // the spec's enum shape (Before vs BeforeAfter).
            op.delay = Some(match spec {
                nmbrs_workload::model::DelaySpec::Before(name) => {
                    nmbrs_workload::model::DelaySpec::Before(rewrite(name))
                }
                nmbrs_workload::model::DelaySpec::BeforeAfter { before, after } => {
                    nmbrs_workload::model::DelaySpec::BeforeAfter {
                        before: before.as_deref().map(rewrite),
                        after: after.as_deref().map(rewrite),
                    }
                }
            });
        }
    }

    expr_to_name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_polydat_op(name: &str, stmt: &str, bindings: &str) -> ParsedOp {
        let mut op = ParsedOp::simple(name, stmt);
        op.bindings = BindingsDef::PolydatSource(bindings.to_string());
        op
    }

    #[test]
    fn objective_bare_wire_vs_inline_expression() {
        // Bare identifiers are read directly off the phase kernel.
        assert!(objective_is_bare_wire("score"));
        assert!(objective_is_bare_wire("err_rate"));
        assert!(objective_is_bare_wire("_objective"));
        assert!(objective_is_bare_wire("  recall99  ")); // trimmed
        assert_eq!(objective_wire("score"), "score");
        assert_eq!(objective_wire("  recall99  "), "recall99");

        // Anything with operators / calls / dots / spaces is an inline
        // expression → synthesized to the `__objective` wire.
        assert!(!objective_is_bare_wire("0 - err_rate"));
        assert!(!objective_is_bare_wire("metricsql_scalar(\"x\")"));
        assert!(!objective_is_bare_wire("a - b"));
        assert!(!objective_is_bare_wire("q.cursor.depth"));
        assert!(!objective_is_bare_wire(""));
        assert_eq!(objective_wire("0 - err_rate"), OBJECTIVE_WIRE);
        assert_eq!(objective_wire("metricsql_scalar(\"x\")"), OBJECTIVE_WIRE);
    }

    #[test]
    fn inline_objective_synthesizes_volatile_objective_binding() {
        use nmbrs_workload::model::{BindingsDef, OptimizeBlock, WorkloadPhase};
        let phase = WorkloadPhase {
            for_each: Some("rate in 1000, 2000".to_string()),
            bindings: BindingsDef::PolydatSource("input cycle: u64\n".to_string()),
            optimize: Some(OptimizeBlock {
                method: "sweep".to_string(),
                objective: "0 - metricsql_scalar(\"sum(rate(errors_total[3s]))\")".to_string(),
                servo: vec!["rate".to_string()],
                max_evals: 10,
                seed: 0,
                params: Default::default(),
            }),
            ..Default::default()
        };
        let out = synthesize_phase_scope_bindings(&phase).expect("synthesis ok");
        let src = match out {
            BindingsDef::PolydatSource(s) => s,
            other => panic!("expected PolydatSource, got {other:?}"),
        };
        assert!(
            src.contains("volatile __objective := 0 - metricsql_scalar("),
            "inline objective must be lowered to a `__objective` binding:\n{src}"
        );

        // A bare-name objective is NOT synthesized — read directly.
        let mut bare = phase.clone();
        bare.optimize.as_mut().unwrap().objective = "score".to_string();
        let bare_out = synthesize_phase_scope_bindings(&bare).expect("synthesis ok");
        if let BindingsDef::PolydatSource(s) = bare_out {
            assert!(
                !s.contains("__objective"),
                "bare objective must not synthesize:\n{s}"
            );
        }
    }

    #[test]
    fn inherited_bindings_dedup_across_ops() {
        let bindings = "input cycle: u64\nprofiles := matching_profiles(\"example\", \"label\")";
        let ops = vec![
            make_polydat_op("op_a", "{profiles}", bindings),
            make_polydat_op("op_b", "{profiles}", bindings),
        ];
        let scope = build_scope(
            &ops,
            &HashMap::new(),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .unwrap();
        scope.validate().unwrap();
        let emitted = scope.emit();
        // 'profiles' should appear exactly once
        let count = emitted.matches("profiles :=").count();
        assert_eq!(
            count, 1,
            "expected exactly 1 'profiles :=' in emitted scope, got {count}:\n{emitted}"
        );
    }

    #[test]
    fn iteration_vars_dont_conflict_with_inherited() {
        let bindings = "input cycle: u64\nprofiles := matching_profiles(\"example\", \"label\")";
        let ops = vec![
            make_polydat_op("op_a", "{profiles} {table}", bindings),
            make_polydat_op("op_b", "{profiles} {table}", bindings),
        ];
        let mut iter_vars = HashMap::new();
        iter_vars.insert("table".to_string(), "vec_default".to_string());

        let scope = build_scope(
            &ops,
            &iter_vars,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .unwrap();
        scope.validate().unwrap();
        let emitted = scope.emit();
        // Iteration variables now declare as `extern <name>:
        // <Type>` so the runtime can populate them per iteration
        // without recompiling (SRD 18b §"Iteration variables as
        // scope outputs"). Type is inferred from the value;
        // "vec_default" doesn't parse numerically → String.
        assert!(
            emitted.contains("extern table: String"),
            "expected extern table declaration in:\n{emitted}"
        );
        assert!(
            emitted.contains("profiles :="),
            "expected profiles in:\n{emitted}"
        );
    }

    #[test]
    fn op_augmentation_adds_new_names() {
        let base = "input cycle: u64\nfoo := hash(cycle)";
        let augmented = "input cycle: u64\nfoo := hash(cycle)\nbar := mod(cycle, 100)";
        let ops = vec![
            make_polydat_op("op_a", "{foo}", base),
            make_polydat_op("op_b", "{foo} {bar}", augmented),
        ];
        let scope = build_scope(
            &ops,
            &HashMap::new(),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .unwrap();
        scope.validate().unwrap();
        let emitted = scope.emit();
        assert!(emitted.contains("foo := hash(cycle)"), "missing foo");
        assert!(emitted.contains("bar := mod(cycle, 100)"), "missing bar");
    }

    #[test]
    fn real_shadow_is_caught() {
        let base = "input cycle: u64\nfoo := hash(cycle)";
        let shadow = "input cycle: u64\nfoo := mod(cycle, 100)";
        let ops = vec![
            make_polydat_op("op_a", "{foo}", base),
            make_polydat_op("op_b", "{foo}", shadow),
        ];
        let scope = build_scope(
            &ops,
            &HashMap::new(),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .unwrap();
        let result = scope.validate();
        assert!(result.is_err(), "expected shadow error");
        let err = result.unwrap_err();
        assert!(
            err.contains("shadows"),
            "expected 'shadows' in error: {err}"
        );
        assert!(err.contains("op_b"), "expected op name in error: {err}");
    }

    #[test]
    fn original_bug_repro_no_false_shadow() {
        // The original bug: workload-level bindings with profiles,
        // inherited by a phase with for_each iteration vars.
        // Two ops share identical inherited bindings.
        // The init injection used to make them differ, causing false shadow.
        let bindings = "input cycle: u64\nprofiles := matching_profiles(\"example\", \"label\")";
        let ops = vec![
            make_polydat_op(
                "drop_metadata_index",
                "DROP INDEX {table}_meta_idx",
                bindings,
            ),
            make_polydat_op("drop_vector_index", "DROP INDEX {table}_idx", bindings),
            make_polydat_op("drop_table", "DROP TABLE {table}", bindings),
        ];
        let mut iter_vars = HashMap::new();
        iter_vars.insert("table".to_string(), "fknn_default".to_string());
        iter_vars.insert("spec".to_string(), "example:default".to_string());
        iter_vars.insert("optimize_for".to_string(), "RECALL".to_string());

        let scope = build_scope(
            &ops,
            &iter_vars,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .unwrap();
        // This was the bug: validate() used to fail with false shadow error
        scope.validate().unwrap();
        let emitted = scope.emit();
        // Iter vars now declare as extern (SRD 18b). The original
        // bug — false-shadow detection — is unchanged regardless
        // of how the iter var materialises.
        assert!(
            emitted.contains("extern table: String"),
            "missing extern table in:\n{emitted}"
        );
        assert!(emitted.contains("profiles :="), "missing profiles");
        // profiles should appear exactly once
        let count = emitted.matches("profiles :=").count();
        assert_eq!(count, 1, "profiles duplicated in:\n{emitted}");
    }

    // ── SRD-13d Phase 9 cross-scope contract check ──────────

    fn parent_kernel_with_load() -> polydat::kernel::PolydatKernel {
        // Parent has `cycle` input, a folded constant `dim`, a
        // shared output `budget`, and a dynamic output `load`
        // (cycle-dependent, no modifier). Each shape exercises
        // a different `ParentRefKind` arm.
        polydat::dsl::compile::compile_polydat(
            "input cycle: u64\n\
             const dim := 128\n\
             shared budget := 100\n\
             load := add(cycle, 1)\n",
        )
        .expect("compile parent")
    }

    fn op_with_body(name: &str, body: &str) -> ParsedOp {
        let mut op = ParsedOp::simple(name, "noop");
        op.bindings = BindingsDef::PolydatSource(body.into());
        op
    }

    #[test]
    fn op_template_referencing_cycle_input_is_accepted() {
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        let op = op_with_body("step_op", "step := add(cycle, 1)\n");
        let result = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "test",
        );
        assert!(
            result.is_ok(),
            "cycle is a parent input — should be accepted. err: {:?}",
            result.err()
        );
    }

    /// Localises the metric-wiring defect: when an op-template kernel is
    /// materialised, does the synthesised `__metric_<name>` binding surface as
    /// an output on THAT kernel? The metrics wrapper reads it through
    /// `ctx.wires`, so if it is absent the wrapper reports
    /// "did not resolve through ctx.wires".
    #[test]
    fn op_template_kernel_exposes_synthesised_metric_bindings() {
        use nmbrs_workload::model::MetricSpec;
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        let mut op = op_with_body("m_op", "measured := add(cycle, 1)\n");
        op.metrics.insert(
            "bytes_out".to_string(),
            MetricSpec {
                value: "measured".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        let kernel = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "test",
        )
        .expect("op-template kernel builds");
        let outs = kernel.program().output_names();
        assert!(
            outs.contains(&"__metric_bytes_out"),
            "the synthesised metric binding must be an output of the \
             op-template kernel; outputs: {outs:?}"
        );
    }

    #[test]
    fn op_template_referencing_constant_output_is_accepted() {
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        // `dim` is a `final` (folded) output — snapshot is final,
        // per-cycle changes are impossible by construction.
        let op = op_with_body("calc_op", "scaled := mul(dim, 2)\n");
        let result = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "test",
        );
        assert!(
            result.is_ok(),
            "final/folded output should be accepted. err: {:?}",
            result.err()
        );
    }

    #[test]
    fn op_template_referencing_shared_output_is_accepted() {
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        // `budget` is `shared` — SharedCell carries live updates.
        let op = op_with_body("budget_op", "remaining := add(budget, 1)\n");
        let result = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "test",
        );
        assert!(
            result.is_ok(),
            "shared output should be accepted. err: {:?}",
            result.err()
        );
    }

    #[test]
    fn op_template_referencing_dynamic_output_accepted_per_srd_13f() {
        // SRD-13f retires SRD-13c's "DynamicOutput without
        // shared" rejection. The read invariant is uniform —
        // inner reads of cross-scope wires return outer's
        // current value via construction-time wiring (cells
        // for shared, per-cycle refresh / planned cell
        // mechanism for non-shared). The op-template
        // synthesiser accepts the reference; the wiring keeps
        // the invariant intact at cycle time.
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        let op = op_with_body("forecast_op", "forecast := mul(load, 2)\n");
        let kernel = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "test",
        )
        .expect("op-template kernel synth should accept dynamic parent ref");
        // The op-template kernel carries `load` as an extern
        // input — the construction-time wiring set up the slot;
        // per-cycle refresh keeps it current with outer.
        assert!(
            kernel.program().find_input("load").is_some(),
            "extern load slot should land on op-template kernel"
        );
        // And `forecast`, the op-local binding, is an output.
        assert!(
            kernel.program().output_names().contains(&"forecast"),
            "op-local binding should be an output"
        );
    }

    #[test]
    fn op_template_pvs_query_full_shape_with_workload_params() {
        // Closer to the actual full_cql_vector.yaml shape: the
        // op carries `prepared:` text with `{keyspace}.{table}`
        // / `{predicate}` / `{query_vector}` / `{limit}` interp
        // bind-points, plus a `metrics: overscan: {value:
        // overscan}` declaration. workload_params carries
        // dataset/profile/keyspace via `final` injection. The
        // op-template synthesiser must emit the latency_factor /
        // recall_factor / overscan bindings as outputs so the
        // metrics fixture's `register_pull("overscan")`
        // resolves.
        use nmbrs_workload::model::MetricSpec;
        let parent_src = r#"
extern k: u64
extern limit: u64
extern optimize_for: String
extern table: String
"#;
        let parent = polydat::dsl::compile::compile_polydat_with_options(
            parent_src,
            &polydat::dsl::compile::CompileOptions {
                context: "parent".to_string(),
                ..Default::default()
            },
            None,
        )
        .expect("parent compile");
        let manifest: Vec<crate::runner::ManifestEntry> =
            polydat::kernel::extract_manifest(parent.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let body = "const prebuffered := dataset_prebuffer(\"{dataset}:{profile}\")\n\
            const query_counts := query_count(prebuffered)\n\
            cursor q = range(0, query_counts * 10)\n\
            query_vector := query_vector_at(prebuffered, q % query_counts)\n\
            predicate := predicate_value_at(prebuffered, q % query_counts)\n\
            ground_truth := filtered_neighbor_indices_at(prebuffered, q % query_counts)\n\
            latency_factor := 0.979 + 4.021 * pow(limit, -0.761)\n\
            recall_factor  := 0.509 + 9.491 * pow(limit, -0.402)\n\
            overscan := if(optimize_for == \"LATENCY\", latency_factor, recall_factor)\n";
        let mut op = op_with_body("select_ann", body);
        // Mirror op fields the YAML carries.
        op.op.insert(
            "prepared".into(),
            serde_json::json!(
                "SELECT key,value FROM {keyspace}.{table} \
             WHERE metadata = {predicate} \
             ORDER BY value ANN OF {query_vector} LIMIT {limit}"
            ),
        );
        op.metrics.insert(
            "overscan".into(),
            MetricSpec {
                value: "overscan".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        let mut workload_params = HashMap::new();
        workload_params.insert("dataset".into(), "sift1m".into());
        workload_params.insert("profile".into(), "label_00".into());
        workload_params.insert("keyspace".into(), "baselines".into());
        let kernel = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &workload_params,
            vec![],
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "pvs_query.select_ann",
        )
        .expect("op-template kernel synth");
        let outs: Vec<String> = kernel
            .program()
            .output_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for required in &["overscan", "latency_factor", "recall_factor"] {
            assert!(
                outs.iter().any(|o| o == required),
                "op-template kernel missing '{required}'; outputs: {outs:?}"
            );
        }
        // Workload params must be folded in as `final` constants
        // (not externs) so `init prebuffered =
        // dataset_prebuffer("{dataset}:{profile}")` folds at
        // compile time. If they cascade through as externs the
        // init binding stays unfolded, the per-fiber state never
        // gets the seeded Handle, and downstream nodes (like
        // `neighbor_indices_at(prebuffered, q)`) panic at runtime
        // with `expected Handle, got None/U64`.
        for param in &["dataset", "profile", "keyspace"] {
            // The param either folds out completely (no input
            // slot) or appears as a folded constant — both
            // are fine. What's NOT fine is showing up as an
            // input on the inner kernel.
            assert!(
                kernel.program().find_input(param).is_none(),
                "workload param '{param}' must NOT be an extern input \
                 on the op-template kernel — cascade should emit \
                 it as `final` so init bindings fold. Inputs: {:?}",
                kernel.program().input_names(),
            );
        }
    }

    #[test]
    fn op_template_with_pow_and_if_keeps_all_outputs() {
        // Mirror the shape of full_cql_vector.yaml's pvs_query
        // phase body (after parser merge): init+cursor+:= ladder
        // ending with `pow()` + `if()` bindings. The op-template
        // kernel must expose every `:=` binding as an output;
        // a missing `overscan` is what triggers the
        // `register_pull("overscan")` failure at MetricsDispenser
        // wrap time.
        let parent_src = r#"
extern k: u64
extern limit: u64
extern optimize_for: String
extern table: String
extern dataset: String
extern profile: String
extern keyspace: String
"#;
        let parent = polydat::dsl::compile::compile_polydat_with_options(
            parent_src,
            &polydat::dsl::compile::CompileOptions {
                context: "parent".to_string(),
                ..Default::default()
            },
            None,
        )
        .expect("parent compile");
        let manifest: Vec<crate::runner::ManifestEntry> =
            polydat::kernel::extract_manifest(parent.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let body = "const prebuffered := dataset_prebuffer(\"dummy:default\")\n\
            const query_counts := query_count(prebuffered)\n\
            cursor q = range(0, query_counts * 10)\n\
            query_vector := query_vector_at(prebuffered, q % query_counts)\n\
            predicate := predicate_value_at(prebuffered, q % query_counts)\n\
            ground_truth := filtered_neighbor_indices_at(prebuffered, q % query_counts)\n\
            latency_factor := 0.979 + 4.021 * pow(limit, -0.761)\n\
            recall_factor  := 0.509 + 9.491 * pow(limit, -0.402)\n\
            overscan := if(optimize_for == \"LATENCY\", latency_factor, recall_factor)\n";
        let op = op_with_body("select_ann", body);
        let kernel = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "pvs_query.select_ann",
        )
        .expect("op-template kernel synth");
        let outs: Vec<String> = kernel
            .program()
            .output_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for required in &[
            "query_vector",
            "predicate",
            "ground_truth",
            "latency_factor",
            "recall_factor",
            "overscan",
        ] {
            assert!(
                outs.iter().any(|o| o == required),
                "op-template kernel missing '{required}'; outputs: {outs:?}"
            );
        }
    }

    #[test]
    fn op_template_relevancy_k_r_bare_wire_names_cascade() {
        // Post-SRD-68 follow-up: `evaluations.relevancy.{k, r,
        // expected, actual}` accept bare wire-name forms. The
        // op-template synthesiser must include those names in its
        // cascaded-extern set so the dispenser's canonical kernel
        // can resolve them at wrap-time (where parse_count_param
        // calls wires.get(name)).
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        let mut op = ParsedOp::simple("read", "noop");
        op.bindings = BindingsDef::PolydatSource("".into());
        op.params.insert(
            "relevancy".into(),
            serde_json::json!({
                "actual": "rows",
                "expected": "ground_truth",
                "k": "k_value",
                "r": "limit_value",
                "functions": ["recall"],
            }),
        );
        // Parent has `cycle` as the coord input. The relevancy
        // wire names (rows, ground_truth, k_value, limit_value)
        // aren't on the parent; the synthesiser should still add
        // them to the cascade-extern set so the op-template kernel
        // *declares* them (errors only fire at wrap-time when the
        // canonical kernel actually tries to resolve them).
        //
        // For this unit test we just confirm the synthesiser
        // doesn't error AND that the input/output names list
        // includes each relevancy wire name — meaning the
        // referenced-cascade walker picked them up. We pick names
        // the parent doesn't have so the auto-extern path fires
        // and the resulting kernel has them as inputs.
        let _ = manifest;
        // Use the public synthesis entry point to construct the
        // op-template kernel under a parent that *does* have the
        // referenced wires (so the cascade succeeds).
        let kernel_src = "\
            input cycle: u64\n\
            const rows := 10\n\
            const ground_truth := \"1,2,3\"\n\
            const k_value := 5\n\
            const limit_value := 100\n";
        let real_parent =
            polydat::dsl::compile::compile_polydat(kernel_src).expect("parent compile");
        let real_manifest = polydat::kernel::extract_manifest(real_parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        let kernel = build_op_template_scope_kernel(
            &op,
            &real_manifest,
            &real_parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "relevancy-cascade-test",
        )
        .expect("op-template kernel synth");

        // Each bare-name relevancy wire should resolve through the
        // op-template kernel's lookup — the same path validation.rs
        // takes at wrap-time via canonical_kernel().lookup(name).
        for name in &["rows", "ground_truth", "k_value", "limit_value"] {
            assert!(
                kernel.lookup(name).is_some(),
                "relevancy wire '{name}' should be visible on the \
                 op-template kernel (cascaded extern); kernel had \
                 outputs: {outs:?}",
                outs = kernel.program().output_names()
            );
        }
    }

    #[test]
    fn op_template_metric_value_count_allocates_magic_extern_slot() {
        // Post-SRD-68 follow-up: a metric `value:` expression is a
        // use site for any names it references. The op-template
        // kernel synthesiser appends `__metric_<name> := <value_expr>`
        // to the result-bindings source, so the closure-binding
        // economy's free-identifier walker sees `count` and injects
        // an `extern count: u64` slot — no throw-away result-binding
        // needed.
        //
        // Verifies: a workload with NO `result:` block but a
        // `metrics: rows_per_op: { value: count }` declaration ends
        // up with a `count` INPUT slot on the kernel.
        let parent = parent_kernel_with_load();
        let manifest = polydat::kernel::extract_manifest(parent.program())
            .into_iter()
            .map(|e| crate::runner::ManifestEntry {
                name: e.name,
                port_type: e.port_type,
                modifier: e.modifier,
            })
            .collect::<Vec<_>>();
        let mut op = ParsedOp::simple("read", "noop");
        op.bindings = BindingsDef::PolydatSource("".into());
        op.metrics.insert(
            "rows_per_op".into(),
            nmbrs_workload::model::MetricSpec {
                value: "count".into(),
                family: None,
                kind: Some(nmbrs_workload::model::MetricKind::Gauge),
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        let kernel = build_op_template_scope_kernel(
            &op,
            &manifest,
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            polydat::kernel::KernelOptLevel::Release,
            "metric-walker-test",
        )
        .expect("op-template kernel synth");
        let inputs = kernel.program().input_names();
        assert!(
            inputs.iter().any(|i| i == "count"),
            "metric `value: count` should force the `count` magic-extern \
             input slot to be allocated under Release opt level; \
             inputs were: {inputs:?}"
        );
        // And the synthesised binding shows up as an output the
        // MetricsDispenser will read at cycle time.
        let outs = kernel.program().output_names();
        let synth = synthesize_metric_binding_name("rows_per_op");
        assert!(
            outs.iter().any(|o| o == &synth),
            "synthesised `{synth}` binding should be a kernel output; \
             outputs were: {outs:?}"
        );
    }

    #[test]
    fn promoted_final_emits_inline_literal_for_str() {
        // SRD-13f case 1 — when a referenced name is `final`
        // upstream and a Str, the synthesizer emits
        // `const name := "value"` in the child's source rather
        // than auto-externing it.
        let parent =
            polydat::dsl::compile_polydat("input cycle: u64\nconst dataset := \"sift1m\"\n")
                .expect("compile parent");
        let manifest: Vec<crate::runner::ManifestEntry> =
            polydat::kernel::extract_manifest(parent.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let ops = vec![make_polydat_op("step", "x={dataset}", "input cycle: u64")];
        let scope = build_scope(
            &ops,
            &HashMap::new(),
            &manifest,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            Some(&parent),
        )
        .expect("build_scope");
        let emitted = scope.emit();
        assert!(
            emitted.contains("const dataset := \"sift1m\""),
            "expected promoted-final emission, got:\n{emitted}"
        );
        assert!(
            !emitted.contains("extern dataset"),
            "expected no extern for promoted-final dataset, got:\n{emitted}"
        );
    }

    #[test]
    fn promoted_final_emits_inline_literal_for_u64() {
        let parent = polydat::dsl::compile_polydat("input cycle: u64\nconst count := 42\n")
            .expect("compile parent");
        let manifest: Vec<crate::runner::ManifestEntry> =
            polydat::kernel::extract_manifest(parent.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let ops = vec![make_polydat_op("step", "n={count}", "input cycle: u64")];
        let scope = build_scope(
            &ops,
            &HashMap::new(),
            &manifest,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            Some(&parent),
        )
        .expect("build_scope");
        let emitted = scope.emit();
        assert!(
            emitted.contains("const count := 42"),
            "expected promoted-final u64 emission, got:\n{emitted}"
        );
    }

    #[test]
    fn unresolved_wire_reference_surfaces_validation_error() {
        // SRD-13f case 4 — a typo in a `{...}` placeholder (here
        // `{tirp}` instead of the declared `trip`) is rejected
        // at the synthesizer level with a structured error,
        // not via a downstream Polydat compiler error.
        let parent =
            polydat::dsl::compile_polydat("input cycle: u64\nconst dataset := \"sift1m\"\n")
                .expect("compile parent");
        let manifest: Vec<crate::runner::ManifestEntry> =
            polydat::kernel::extract_manifest(parent.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let ops = vec![make_polydat_op("step", "x={tirp}", "input cycle: u64")];
        let err = match build_scope(
            &ops,
            &HashMap::new(),
            &manifest,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            Some(&parent),
        ) {
            Ok(_) => panic!("expected unresolved-wire error, got Ok"),
            Err(e) => e,
        };
        assert!(err.contains("unresolved wire"), "wrong error: {err}");
        assert!(
            err.contains("tirp"),
            "error should mention the typoed name: {err}"
        );
        assert!(
            err.contains("Visible names"),
            "error should list visible names: {err}"
        );
    }

    #[test]
    fn ingest_preserves_bindings_when_comment_contains_apostrophe() {
        // Regression: Polydat grammar uses only `"`-delimited string
        // literals. `logical_lines` previously treated `'` as a
        // string delimiter too, which meant an unmatched
        // apostrophe in a `#` comment (e.g. "the workload's
        // bindings") put the splitter into a string state that
        // never closed. The entire rest of the source was
        // accreted into one logical "line" starting with `#`,
        // which then got dropped as a comment by the per-line
        // parser — silently discarding every binding that
        // followed.
        //
        // This was the actual root cause of the full_cql_vector
        // `await_index` failure: the workload's
        // `bindings:` block began with a multi-line comment
        // containing the apostrophe in "full_cql_vector's", so
        // the `shared has_X := false` declarations never made
        // it into the workload-root program. Descendant
        // synthesizers then couldn't cascade has_X as Bool
        // externs; the compile inferred has_X as Coordinate
        // U64 inputs from unbound references, and the
        // per-cycle `set_inputs(&[u64])` clobbered the
        // SharedCell with `Value::U64(cycle)`.
        let mut scope = BindingScope::new();
        let src_with_apostrophe_comment = "# full_cql_vector's bindings block\n\
                                           shared has_a := true\n\
                                           shared has_b := false\n";
        scope.ingest_polydat_source(src_with_apostrophe_comment, BindingOrigin::Inherited);
        let defined = scope.defined_names();
        assert!(
            defined.contains("has_a"),
            "comment with apostrophe must NOT consume subsequent bindings; \
             expected has_a in defined names, got {defined:?}"
        );
        assert!(
            defined.contains("has_b"),
            "expected has_b in defined names, got {defined:?}"
        );

        let emitted = scope.emit();
        assert!(
            emitted.contains("shared has_a := true"),
            "scope.emit() must include the shared bindings; got:\n{emitted}"
        );
        assert!(
            emitted.contains("shared has_b := false"),
            "scope.emit() must include the shared bindings; got:\n{emitted}"
        );
    }

    #[test]
    fn ingest_then_compile_preserves_shared_modifier() {
        // Mirrors the workload-root compile path in
        // `compile_bindings_with_libs_excluding`: workload-level
        // bindings (`shared has_X := false`) get ingested via
        // `scope.ingest_polydat_source(..., Inherited)` before emit.
        // The resulting source then compiles via the
        // standard pipeline.
        //
        // SRD-13c §"Shared Mutable": `shared X := <literal>`
        // compiles to an input slot + passthrough output marked
        // SHARED. The workload-root program's `shared_outputs()`
        // must include the SHARED-modifier outputs so
        // `seed_shared_cells` creates a `SharedCell` for each
        // and descendant kernels can cell-attach via
        // `materialize_wiring_from_outer`.
        let mut scope = BindingScope::new();
        let workload_polydat = "shared has_sai_column_indexes := false\n\
                          shared has_indexes := false\n";
        scope.ingest_polydat_source(workload_polydat, BindingOrigin::Inherited);
        let source = scope.emit();
        let kernel = polydat::dsl::compile_polydat(&source)
            .unwrap_or_else(|e| panic!("compile failed for source:\n{source}\nerror: {e}"));
        let shared = kernel.program().shared_outputs();
        assert!(
            shared.contains(&"has_sai_column_indexes"),
            "expected `has_sai_column_indexes` in shared_outputs after scope-ingest/emit/compile;\n\
             got shared_outputs={shared:?}\nemitted source:\n{source}",
        );
        assert!(
            shared.contains(&"has_indexes"),
            "expected `has_indexes` in shared_outputs after scope-ingest/emit/compile;\n\
             got shared_outputs={shared:?}\nemitted source:\n{source}",
        );
    }

    /// SRD-75 Push 2: `synthesize_phase_scope_bindings`
    /// returns the original bindings unchanged when
    /// `phase.poll` is `None`. The function is a no-op for
    /// non-poll phases; their synthesis path is unaffected.
    #[test]
    fn synthesize_phase_scope_bindings_passthrough_when_no_poll() {
        use nmbrs_workload::model::{BindingsDef, WorkloadPhase};
        let phase = WorkloadPhase {
            bindings: BindingsDef::PolydatSource("k := 5\n".into()),
            poll: None,
            ..Default::default()
        };
        let out = synthesize_phase_scope_bindings(&phase).expect("no-poll synthesis is a no-op");
        match out {
            BindingsDef::PolydatSource(s) => assert_eq!(
                s, "k := 5\n",
                "no-poll should return the original bindings unchanged"
            ),
            other => panic!("expected PolydatSource, got {other:?}"),
        }
    }

    /// A metric's `cell:` is reified as a compiled kernel binding beside its
    /// value — the whole point of the design. If this did not compile, a
    /// coordinate would have to be a runtime string, which is the
    /// unvalidatable shape the proposal exists to avoid.
    #[test]
    fn phase_metrics_reify_cell_coordinates_as_kernel_bindings() {
        use nmbrs_workload::model::{BindingsDef, MetricSpec, WorkloadPhase};
        let mut metrics = std::collections::HashMap::new();
        let mut cell = std::collections::BTreeMap::new();
        cell.insert("tier".to_string(), "tier_name".to_string());
        metrics.insert(
            "bytes_out".to_string(),
            MetricSpec {
                value: "history_bytes_out".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell,
            },
        );
        let phase = WorkloadPhase {
            bindings: BindingsDef::PolydatSource(
                "extern tier_name: str = \"\"\n\
                 extern history_bytes_out: u64 = 0\n"
                    .into(),
            ),
            metrics,
            poll: None,
            ..Default::default()
        };
        let out = synthesize_phase_scope_bindings(&phase).expect("synthesis");
        let src = match out {
            BindingsDef::PolydatSource(s) => s,
            other => panic!("expected PolydatSource, got {other:?}"),
        };
        assert!(
            src.contains("volatile __cell_bytes_out__tier := tier_name"),
            "coordinate must be emitted as a volatile binding; got:\n{src}"
        );
        assert!(
            src.contains("volatile __metric_bytes_out := history_bytes_out"),
            "the value binding must still be emitted; got:\n{src}"
        );
    }

    /// Per (metric, dimension), not per dimension: two metrics placing into
    /// one dimension by different expressions must not collide on a wire name
    /// and silently let one placement win.
    #[test]
    fn two_metrics_in_one_dimension_get_distinct_coordinate_wires() {
        assert_eq!(
            synthesize_cell_binding_name("bytes_out", "tier"),
            "__cell_bytes_out__tier"
        );
        assert_ne!(
            synthesize_cell_binding_name("bytes_out", "tier"),
            synthesize_cell_binding_name("bytes_in", "tier")
        );
    }

    /// A phase with a `metrics:` block (and no poll) synthesises a
    /// `volatile phase_start := phase_start_millis()` origin binding plus one
    /// `volatile __metric_<name> := <value>` per metric. The phase's own
    /// bindings are appended verbatim.
    ///
    /// `phase_start` was an `extern … = 0` filled by the executor at the
    /// completion-time pull, which meant anything reading it while the phase
    /// ran got the default.
    #[test]
    fn synthesize_phase_scope_bindings_emits_metric_bindings() {
        use nmbrs_workload::model::{BindingsDef, MetricSpec, WorkloadPhase};
        let mut metrics = std::collections::HashMap::new();
        metrics.insert(
            "time_to_index".to_string(),
            MetricSpec {
                value: "current_epoch_millis() - phase_start".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        let phase = WorkloadPhase {
            bindings: BindingsDef::default(),
            metrics,
            poll: None,
            ..Default::default()
        };
        let out = synthesize_phase_scope_bindings(&phase).expect("metrics synthesis");
        let src = match out {
            BindingsDef::PolydatSource(s) => s,
            other => panic!("expected PolydatSource, got {other:?}"),
        };
        // Bound to the runtime's phase clock, NOT an extern: as an extern it
        // was filled only at the completion-time pull, so anything reading it
        // while the phase ran (an op) got the 0 default and computed against
        // the epoch.
        assert!(
            src.contains("volatile phase_start := phase_start_millis()"),
            "must bind phase_start to the phase-scoped clock; got:\n{src}"
        );
        assert!(
            !src.contains("extern phase_start"),
            "the fill-me-in extern must be gone; got:\n{src}"
        );
        assert!(
            src.contains("volatile __metric_time_to_index := current_epoch_millis() - phase_start"),
            "must emit the volatile metric binding; got:\n{src}"
        );
        // The emitted source must compile as a phase kernel body, with
        // `__metric_time_to_index` surfacing as an output.
        let kernel = polydat::dsl::compile_polydat(&src)
            .unwrap_or_else(|e| panic!("compile failed:\n{src}\nerror: {e}"));
        assert!(
            kernel
                .program()
                .output_names()
                .contains(&"__metric_time_to_index"),
            "metric binding must be a kernel output; outputs: {:?}",
            kernel.program().output_names()
        );
    }

    /// SRD-75 Push 2: when a phase has `poll:` and ops with
    /// declared captures, the synthesized source carries one
    /// `shared <name>: u64 := 0` declaration per capture
    /// followed by the predicate binding `__poll_until :=
    /// <until>`. The original phase bindings (if any) are
    /// appended verbatim between the captures and the
    /// predicate.
    #[test]
    fn synthesize_phase_scope_bindings_emits_shared_captures_and_predicate() {
        use nmbrs_workload::bindpoints::CapturePoint;
        use nmbrs_workload::model::{BindingsDef, ParsedOp, PhasePollSpec, WorkloadPhase};
        let mut op = ParsedOp::simple("read_state", "noop");
        op.captures = vec![
            CapturePoint {
                row_filter: None,
                source_name: "sstables".into(),
                as_name: "sstables".into(),
                cast_type: None,
                slurp: false,
                path: Some("/0/value".into()),
                count: false,
                agg: None,
            },
            CapturePoint {
                row_filter: None,
                source_name: "active_for_cf".into(),
                as_name: "active_for_cf".into(),
                cast_type: None,
                slurp: false,
                path: Some("/1/value".into()),
                count: true,
                agg: None,
            },
        ];
        let phase = WorkloadPhase {
            ops: vec![op],
            bindings: BindingsDef::PolydatSource("dummy := 1\n".into()),
            poll: Some(PhasePollSpec {
                until: "sstables == 1 && active_for_cf == 0".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = synthesize_phase_scope_bindings(&phase).expect("poll synthesis should succeed");
        let src = match out {
            BindingsDef::PolydatSource(s) => s,
            other => panic!("expected PolydatSource, got {other:?}"),
        };
        assert!(
            src.contains("shared sstables := 0"),
            "missing shared declaration for sstables; source:\n{src}"
        );
        assert!(
            src.contains("shared active_for_cf := 0"),
            "missing shared declaration for active_for_cf; source:\n{src}"
        );
        assert!(
            src.contains("dummy := 1"),
            "original phase bindings should appear verbatim; source:\n{src}"
        );
        assert!(
            src.contains("__poll_until := sstables == 1 && active_for_cf == 0"),
            "missing __poll_until predicate binding; source:\n{src}"
        );
        // Order matters: captures first (so the predicate's
        // RHS references resolve through the local shared
        // declarations before the dynamic binding evaluates).
        let shared_pos = src
            .find("shared sstables")
            .expect("shared sstables present");
        let until_binding = crate::wrappers::condition::UNTIL_BINDING;
        let until_pos = src
            .find(until_binding)
            .expect("poll predicate binding present");
        assert!(
            shared_pos < until_pos,
            "shared captures must precede __poll_until in the synthesized source"
        );
    }

    /// SRD-75 Push 2: a capture name that collides with an
    /// author-declared phase binding is a workload bug —
    /// the synthesizer surfaces the collision rather than
    /// silently shadowing.
    #[test]
    fn synthesize_phase_scope_bindings_rejects_capture_name_collision() {
        use nmbrs_workload::bindpoints::CapturePoint;
        use nmbrs_workload::model::{BindingsDef, ParsedOp, PhasePollSpec, WorkloadPhase};
        let mut op = ParsedOp::simple("read_state", "noop");
        op.captures = vec![CapturePoint {
            row_filter: None,
            source_name: "sstables".into(),
            as_name: "sstables".into(),
            cast_type: None,
            slurp: false,
            path: Some("/0/value".into()),
            count: false,
            agg: None,
        }];
        // Phase author also declared `sstables := …` —
        // collision is the bug.
        let phase = WorkloadPhase {
            ops: vec![op],
            bindings: BindingsDef::PolydatSource("sstables := 7\n".into()),
            poll: Some(PhasePollSpec {
                until: "sstables == 1".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err =
            synthesize_phase_scope_bindings(&phase).expect_err("colliding capture name must error");
        assert!(
            err.contains("sstables") && err.contains("collision"),
            "expected error to name 'sstables' and 'collision'; got: {err}"
        );
    }

    #[test]
    fn build_phase_scope_kernel_with_mod_in_binding_over_partition_iter_var() {
        // Matches the production workload shape:
        //   for: "p in partitions(\"linear:3\")"
        //   phases.walk.bindings: n := mod_in(cycle, p)
        //
        // The phase compile must see `p` as Ext so mod_in's
        // second arg (Partition input) type-checks. Reproduces
        // the integration error "u64 ─▶ ext expects ext".
        let parent_kernel = crate::scope_synth::build_for_each_scope_kernel(
            &[("p".to_string(), "partitions(\"linear:3\")".to_string())],
            &[],
            &polydat::dsl::compile_polydat("\n").unwrap(),
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_for_each",
            None,
        )
        .expect("for-each scope synthesis");

        let phase_bindings = nmbrs_workload::model::BindingsDef::PolydatSource(
            "n := mod_in(cycle, p)\n".to_string(),
        );
        let phase_kernel = build_phase_scope_kernel(
            &phase_bindings,
            &[],
            &parent_kernel,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_phase",
        )
        .expect("phase kernel build with mod_in(cycle, p) binding");

        // Sanity: phase has p as Ext-typed input slot.
        assert_eq!(
            phase_kernel.program().input_port_type("p"),
            Some(polydat::ast::PortType::Ext),
            "phase kernel's `p` must be Ext-typed",
        );
        // Sanity: phase has `n` as an output (the mod_in result).
        assert!(
            phase_kernel.program().output_names().contains(&"n"),
            "phase kernel should expose `n` as an output"
        );
    }

    #[test]
    fn build_phase_scope_kernel_cascades_partition_iter_var_as_extern_ext() {
        // SRD-71: when a for-each scope iterates a PartitionList
        // and binds an iter-var `p`, descendant phases must see
        // `p` as `extern p: Ext` — NOT cast to String / u64 by
        // the cascade. The phase body's `over p` clause then
        // resolves `p` as a partition-typed wire.
        //
        // Reproduces the integration failure where the phase's
        // build_phase_scope_kernel cascade was emitting
        // `extern p: u64` (or Str fallback) because the
        // type-name lookup didn't handle PortType::Ext.
        let parent_kernel = crate::scope_synth::build_for_each_scope_kernel(
            &[("p".to_string(), "partitions(\"linear:3\")".to_string())],
            &[], // empty parent_manifest is fine; for_each scope only
            // cascades names it actually references.
            &polydat::dsl::compile_polydat("\n").unwrap(),
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_for_each",
            None,
        )
        .expect("for-each scope synthesis");

        // Sanity: the for-each scope's iter-var p is Ext-typed.
        assert_eq!(
            parent_kernel.program().input_port_type("p"),
            Some(polydat::ast::PortType::Ext),
            "for-each scope's iter-var `p` must declare as Ext",
        );

        // Now build a phase under it that references `p` in a
        // cursor's `over` clause.
        let phase_bindings = nmbrs_workload::model::BindingsDef::PolydatSource(
            "cursor row = range(0, 1000) over p\n".to_string(),
        );
        let phase_kernel = build_phase_scope_kernel(
            &phase_bindings,
            &[],
            &parent_kernel,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_phase",
        )
        .expect("phase kernel build");

        // The phase kernel must see `p` as Ext-typed.
        let port_type = phase_kernel.program().input_port_type("p");
        assert_eq!(
            port_type,
            Some(polydat::ast::PortType::Ext),
            "phase kernel's `p` slot must be Ext (preserved through cascade), got {port_type:?}"
        );
    }

    #[test]
    fn build_phase_scope_kernel_cascades_shared_bool_as_extern_bool() {
        // SRD-13d §1: "The kernel auto-externs every name it
        // doesn't declare locally; those externs resolve up
        // through the phase kernel and beyond per the standard
        // SRD-13c chain."
        //
        // SRD-66 §"Surface 2": a workload-root `shared X :=
        // <literal>` becomes a `SharedCell`-backed slot at the
        // root. Descendant phases declare `extern X: bool` (or
        // auto-extern emits it); bind_outer_scope cell-attaches
        // the slot.
        //
        // SRD-13c §"Shared Mutable" step 1: `shared X := <literal>`
        // produces an input slot Bool (kind=ExternalWrite) plus a
        // passthrough output Bool, modifier SHARED.
        //
        // This unit-test verifies the documented type-preservation
        // contract: a phase scope built via build_phase_scope_kernel
        // against a parent kernel carrying a SHARED Bool output
        // must produce a phase-scope kernel whose has_X input slot
        // is type Bool, NOT typed U64 / not classified Coordinate.
        // Without this property, the SharedCell that bind_outer_scope
        // attaches gets a slot whose declared type doesn't match
        // the cell's actual Value variant — downstream consumers
        // (e.g. pick) see the runtime variant and reject it.
        let parent = polydat::dsl::compile_polydat(
            "input cycle: u64\nshared has_sai_column_indexes := false\n\
             shared has_indexes := false\n",
        )
        .expect("parent compile");
        // The phase has its own bindings block (the await_index shape).
        let phase_bindings = nmbrs_workload::model::BindingsDef::PolydatSource(
            "target_index_table := pick(has_sai_column_indexes, has_indexes, \
             \"a\", \"b\")\n"
                .to_string(),
        );
        let phase_kernel = build_phase_scope_kernel(
            &phase_bindings,
            &[], // outer_manifest (would be populated in real runner, but builder doesn't strictly need it)
            &parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_phase",
        )
        .expect("phase kernel build");

        // The phase kernel must have has_X as an input slot.
        let idx_sai = phase_kernel.program().find_input("has_sai_column_indexes");
        assert!(
            idx_sai.is_some(),
            "phase kernel must have `has_sai_column_indexes` input slot"
        );
        let idx = idx_sai.unwrap();

        // Type must be Bool (per SRD-13c §"Shared Mutable" step 1
        // + SRD-66 §"Reading from a downstream phase").
        let port_type = phase_kernel
            .program()
            .input_port_type("has_sai_column_indexes");
        assert_eq!(
            port_type,
            Some(polydat::ast::PortType::Bool),
            "phase kernel's has_sai_column_indexes slot must be Bool, got {port_type:?}"
        );

        // Kind must NOT be Coordinate — cascaded names are IterationExtern
        // (or ExternalWrite), not Coordinate. Coordinate would put the slot
        // in the set_inputs(&[u64]) propagation, breaking the cell-bound
        // contract per SRD-13c §"Shared Mutable" step 3.
        let kind = phase_kernel.program().input_kind(idx);
        assert_ne!(
            kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "phase kernel's has_sai_column_indexes slot must NOT be Coordinate; got {kind:?}"
        );
    }

    #[test]
    fn executor_build_scope_emits_extern_bool_for_cell_backed_shared_wire() {
        // Reproduces the executor's specific build_scope call
        // sequence for the consume phase:
        //
        //   1. Build the phase scope kernel via build_phase_scope_kernel
        //      (this is what the runner's install pass does).
        //   2. Call build_scope(ops, ..., parent_kernel=phase_scope)
        //      with phase_scope as classifier_kernel — this is
        //      what executor.rs:1429 does at run_phase time.
        //   3. compile_from_scope(scope, ...) — produces the
        //      iter_op_builder kernel that dryrun=wiring dumps.
        //
        // The integration test
        // `shared_bool_through_for_each_into_consumer_phase_bindings`
        // proves this whole sequence produces a kernel with has_X
        // as `coordinate` U64. This unit test reproduces it without
        // a subprocess so we can inspect every intermediate state.
        use polydat::kernel::extract_manifest;

        // ── workload root ──
        let root = polydat::dsl::compile_polydat(
            "shared has_a := true\n\
             shared has_b := false\n\
             selector := mod(cycle, 1)\n",
        )
        .expect("workload root compile");

        // ── for_each scope ──
        let for_each = crate::scope_synth::build_for_each_scope_kernel(
            &[("outer".to_string(), "p1,p2".to_string())],
            &extract_manifest(root.program()),
            &root,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_for_each",
            None,
        )
        .expect("for_each synth");

        // ── phase scope kernel (cached_kernel) ──
        let phase_bindings = nmbrs_workload::model::BindingsDef::PolydatSource(
            "chosen := pick(has_a, has_b, \"alpha\", \"beta\")\n".to_string(),
        );
        let phase_scope = build_phase_scope_kernel(
            &phase_bindings,
            &[],
            &for_each,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_phase",
        )
        .expect("phase scope synth");

        // ── executor's build_scope call ──
        let op = ParsedOp::simple("report", "consume chosen={chosen}");
        let ops = vec![op];
        let effective_manifest: Vec<crate::runner::ManifestEntry> =
            extract_manifest(phase_scope.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let scope = build_scope(
            &ops,
            &HashMap::new(),
            &effective_manifest,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            Some(&phase_scope),
        )
        .expect("executor build_scope");

        // ── inspect what build_scope emits ──
        let emitted = scope.emit();

        // ── compile_from_scope equivalent — uses the SAME compile
        // path the executor takes (compile_polydat_with_options with
        // the scope's required_outputs filter).
        let required = scope.required_outputs();
        let executor_kernel = polydat::dsl::compile::compile_polydat_with_options(
            &emitted,
            &polydat::dsl::compile::CompileOptions {
                required_outputs: required.clone(),
                context: "test".to_string(),
                ..Default::default()
            },
            None,
        )
        .unwrap_or_else(|e| {
            panic!("compile failed: {e}\nemitted source:\n{emitted}\nrequired: {required:?}")
        });

        // The executor's kernel must have has_a as Bool (or absent
        // entirely if not referenced). It MUST NOT be Coordinate U64.
        if let Some(idx) = executor_kernel.program().find_input("has_a") {
            let typ = executor_kernel.program().input_port_type("has_a");
            assert_eq!(
                typ,
                Some(polydat::ast::PortType::Bool),
                "executor kernel's has_a slot must be Bool;\n\
                 got {typ:?}\n\
                 emitted source:\n{emitted}\n\
                 input names: {:?}\n\
                 coord_count: {}",
                executor_kernel.program().input_names(),
                executor_kernel.program().coord_count()
            );
            let kind = executor_kernel.program().input_kind(idx);
            assert_ne!(
                kind,
                Some(polydat::kernel::InputKind::Coordinate),
                "executor kernel's has_a slot must NOT be Coordinate;\n\
                 got {kind:?}\n\
                 emitted source:\n{emitted}\n\
                 input names: {:?}\n\
                 coord_count: {}",
                executor_kernel.program().input_names(),
                executor_kernel.program().coord_count()
            );
        }
    }

    #[test]
    fn full_chain_workload_to_executor_scope_preserves_shared_bool() {
        // Chains through every layer the failing integration test
        // touches, in-process, with the same shape that triggers
        // the failure:
        //
        //   1. params kernel (workload params as `const` bindings).
        //   2. workload-root kernel via the `compile_bindings_with_libs_excluding`-style
        //      flow: build_scope + scope.ingest_polydat_source + parent.build_subscope.
        //   3. for_each scope kernel (outer iter var).
        //   4. for_each scope kernel (inner with multi-iter clause).
        //   5. phase scope kernel via build_phase_scope_kernel (consume).
        //   6. executor's build_scope + compile_polydat_with_libs_and_limit.
        //
        // Verifies at the END that the consumer phase's executor-
        // compiled kernel has has_a as Bool/non-Coordinate.
        use nmbrs_workload::model::ParsedOp;
        use polydat::kernel::extract_manifest;

        // Step 1: params kernel.
        let mut workload_params: HashMap<String, String> = HashMap::new();
        workload_params.insert("dataset".to_string(), "example_dataset".to_string());
        workload_params.insert("prefix".to_string(), "px_".to_string());
        let mut sorted: Vec<&String> = workload_params.keys().collect();
        sorted.sort();
        let mut params_source = String::new();
        for k in sorted {
            params_source.push_str(&format!("const {k} := \"{}\"\n", workload_params[k]));
        }
        let params_kernel = polydat::dsl::compile_polydat(&params_source).expect("params compile");

        // Step 2: workload-root kernel.
        let mut scope = build_scope(
            &[] as &[ParsedOp],
            &HashMap::new(),
            &[],
            &workload_params,
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .expect("workload root build_scope");
        // KEY: the binding RHS uses string interpolation against
        // workload params. This is what triggers the chain
        // corruption — without it, the test passes; with it, the
        // integration test fails.
        let workload_level_polydat = "combo_label := str_concat(\"{dataset}\", \"{prefix}\")\n\
                                 shared has_a := true\n\
                                 shared has_b := false\n";
        scope.ingest_polydat_source(workload_level_polydat, BindingOrigin::Inherited);
        let root_source = scope.emit();
        let root_matter = polydat::kernel::subcontext::PolydatMatter::builder()
            .label("test_root")
            .source(root_source.clone())
            .options(polydat::kernel::subcontext::CompileOptions {
                workload_dir: None,
                polydat_lib_paths: Vec::new(),
                strict: false,
                required_outputs: scope.required_outputs(),
                context_label: Some("test_root".to_string()),
                cursor_limit: None,
                ..Default::default()
            })
            .build()
            .expect("root matter");
        let root = params_kernel
            .build_subscope(root_matter)
            .expect("root build");

        // Sanity: has_a SHARED at root.
        let shared = root.program().shared_outputs();
        assert!(
            shared.contains(&"has_a"),
            "root should have has_a as SHARED output; got {shared:?}"
        );

        // Step 3: outer for_each scope.
        let outer_fe = crate::scope_synth::build_for_each_scope_kernel(
            &[("outer".to_string(), "p1,p2".to_string())],
            &extract_manifest(root.program()),
            &root,
            &workload_params,
            Vec::new(),
            None,
            false,
            "test_outer_fe",
            None,
        )
        .expect("outer for_each synth");

        // Step 4: inner for_each scope (dependent multi-iter).
        let inner_fe = crate::scope_synth::build_for_each_scope_kernel(
            &[
                ("inner".to_string(), "lo,hi".to_string()),
                ("label".to_string(), "tag_p1_lo,tag_p1_hi".to_string()),
            ],
            &extract_manifest(outer_fe.program()),
            &outer_fe,
            &workload_params,
            Vec::new(),
            None,
            false,
            "test_inner_fe",
            None,
        )
        .expect("inner for_each synth");

        // Step 5: phase scope kernel for consume.
        let phase_bindings = BindingsDef::PolydatSource(
            "chosen := pick(has_a, has_b, \"alpha\", \"beta\")\n".to_string(),
        );
        let phase_scope = build_phase_scope_kernel(
            &phase_bindings,
            &[],
            &inner_fe,
            &workload_params,
            Vec::new(),
            None,
            false,
            "test_consume_phase",
        )
        .expect("consume phase synth");

        // Step 6: executor's build_scope on the consume phase.
        let op = ParsedOp::simple("report", "spc/consume chosen={chosen}");
        let ops = vec![op];
        let effective_manifest: Vec<crate::runner::ManifestEntry> =
            extract_manifest(phase_scope.program())
                .into_iter()
                .map(|e| crate::runner::ManifestEntry {
                    name: e.name,
                    port_type: e.port_type,
                    modifier: e.modifier,
                })
                .collect();
        let exec_scope = build_scope(
            &ops,
            &HashMap::new(),
            &effective_manifest,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &[],
            Some(&phase_scope),
        )
        .expect("executor build_scope");

        let exec_source = exec_scope.emit();
        let exec_kernel = polydat::dsl::compile::compile_polydat_with_options(
            &exec_source,
            &polydat::dsl::compile::CompileOptions {
                required_outputs: exec_scope.required_outputs(),
                context: "test_executor".to_string(),
                ..Default::default()
            },
            None,
        )
        .unwrap_or_else(|e| panic!("exec compile failed: {e}\nexec source:\n{exec_source}"));

        // The final kernel — what the dryrun=wiring dumps — must
        // have has_a as Bool/non-Coordinate.
        if let Some(idx) = exec_kernel.program().find_input("has_a") {
            let typ = exec_kernel.program().input_port_type("has_a");
            assert_eq!(
                typ,
                Some(polydat::ast::PortType::Bool),
                "FINAL kernel has_a must be Bool, got {typ:?}\n\
                 exec source:\n{exec_source}\n\
                 exec input_names: {:?}\n\
                 exec coord_count: {}",
                exec_kernel.program().input_names(),
                exec_kernel.program().coord_count()
            );
            let kind = exec_kernel.program().input_kind(idx);
            assert_ne!(
                kind,
                Some(polydat::kernel::InputKind::Coordinate),
                "FINAL kernel has_a must NOT be Coordinate, got {kind:?}\n\
                 exec source:\n{exec_source}\n\
                 exec input_names: {:?}\n\
                 exec coord_count: {}",
                exec_kernel.program().input_names(),
                exec_kernel.program().coord_count()
            );
        }
    }

    #[test]
    fn workload_root_via_compile_bindings_preserves_shared_bool_type() {
        // Mirrors the runner's WORKLOAD-ROOT build flow:
        //   compile_bindings_with_libs_excluding(
        //     parent=params_kernel,
        //     ops=all_ops,
        //     workload_params=<non-empty>,
        //     workload_level_polydat=Some(<the bindings: block>),
        //   )
        //
        // The actual function takes a complex set of args; here
        // we replicate the essential moves to expose what differs
        // from a direct `compile_polydat(source)` invocation.
        //
        // SRD-13c §"Shared Mutable" requires has_a to be ExternalWrite
        // kind, Bool type on the workload-root program. The
        // dryrun=wiring output of the failing integration test shows
        // the downstream phase has has_a as Coordinate U64, so
        // either the workload-root or a downstream synthesizer
        // emits the wrong source for it.
        use nmbrs_workload::model::ParsedOp;

        let mut workload_params: HashMap<String, String> = HashMap::new();
        workload_params.insert("dataset".to_string(), "example_dataset".to_string());
        workload_params.insert("prefix".to_string(), "px_".to_string());
        workload_params.insert("keyspace".to_string(), "ks1".to_string());
        workload_params.insert("inner_options".to_string(), "lo,hi".to_string());

        // Workload params reach the root via a sibling params
        // kernel. Construct one with each param as a final binding.
        let mut params_source = String::new();
        let mut sorted: Vec<&String> = workload_params.keys().collect();
        sorted.sort();
        for k in sorted {
            let v = &workload_params[k];
            params_source.push_str(&format!("const {k} := \"{v}\"\n"));
        }
        let params_kernel = polydat::dsl::compile_polydat(&params_source).expect("params compile");

        // The workload's `bindings:` block. The trigger.
        let workload_level_polydat = "selector := mod(cycle, 1)\n\
                                 shared has_a := true\n\
                                 shared has_b := false\n";

        // Build the workload-root scope as compile_bindings_with_libs_excluding does.
        let mut scope = build_scope(
            &[] as &[ParsedOp],
            &HashMap::new(),
            &[],
            &workload_params,
            &HashMap::new(),
            None,
            &[],
            None,
        )
        .expect("build_scope");
        scope.ingest_polydat_source(workload_level_polydat, BindingOrigin::Inherited);

        // The actual workload-root compile goes through
        // parent.build_subscope. Build the matter and finalize.
        let source = scope.emit();
        let opts = polydat::kernel::subcontext::CompileOptions {
            workload_dir: None,
            polydat_lib_paths: Vec::new(),
            strict: false,
            required_outputs: scope.required_outputs(),
            context_label: Some("test_workload_root".to_string()),
            cursor_limit: None,
            ..Default::default()
        };
        let matter = polydat::kernel::subcontext::PolydatMatter::builder()
            .label("test_workload_root")
            .source(source.clone())
            .options(opts)
            .build()
            .expect("matter build");
        let root = params_kernel
            .build_subscope(matter)
            .expect("workload root build");

        // The workload-root program must have has_a:
        //   - present as an input slot,
        //   - typed Bool (SRD-13c §"Shared Mutable" step 1),
        //   - kind ExternalWrite (NOT Coordinate),
        //   - marked SHARED in output_modifier (so seed_shared_cells fires).
        let has_a_idx = root.program().find_input("has_a").unwrap_or_else(|| {
            panic!(
                "workload root missing has_a input;\n\
                 emitted scope source:\n{source}\n\
                 input names: {:?}",
                root.program().input_names()
            )
        });

        let typ = root.program().input_port_type("has_a");
        assert_eq!(
            typ,
            Some(polydat::ast::PortType::Bool),
            "workload root has_a must be Bool;\n\
             got {typ:?}\n\
             emitted source:\n{source}\n\
             input names: {:?}\n\
             coord_count: {}",
            root.program().input_names(),
            root.program().coord_count()
        );

        let kind = root.program().input_kind(has_a_idx);
        assert_ne!(
            kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "workload root has_a must NOT be Coordinate;\n\
             got {kind:?}\n\
             emitted source:\n{source}\n\
             input names: {:?}\n\
             coord_count: {}",
            root.program().input_names(),
            root.program().coord_count()
        );

        let modifier = root.program().output_modifier("has_a");
        assert_eq!(
            modifier,
            polydat::dsl::ast::BindingModifier::SHARED,
            "workload root has_a output must have SHARED modifier;\n\
             got {modifier:?}"
        );

        let shared = root.program().shared_outputs();
        assert!(
            shared.contains(&"has_a"),
            "workload root must have has_a in shared_outputs (so seed_shared_cells creates a cell);\n\
             got shared_outputs={shared:?}"
        );
    }

    #[test]
    fn shared_bool_survives_when_workload_root_also_has_cycle_binding() {
        // BISECTED INTEGRATION FAILURE: adding any non-shared
        // workload-level binding that references `cycle`
        // (e.g. `selector := mod(cycle, 1)`) alongside the
        // SHARED bool wires causes the consumer phase's phase
        // scope kernel to have has_X classified as Coordinate
        // input slots typed U64 instead of cascaded externs
        // typed Bool.
        //
        // Manifests at runtime as: per-cycle `set_inputs(&[u64])`
        // clobbers the shared cells with `Value::U64(cycle)`,
        // and `pick(has_X, …)` panics with "non-bool type U64".
        //
        // This unit test walks the same scope chain
        // programmatically and pins the contract at each hop:
        //   workload root (shared bool + cycle-referencing binding)
        //     → for_each scope (synthesize_for_each_scope)
        //       → phase scope (build_phase_scope_kernel)
        // Asserts has_a stays Bool + non-Coordinate kind at every
        // layer.
        use polydat::kernel::extract_manifest;

        // Step 1: workload root with shared bool AND a
        // non-shared cycle binding. The trigger.
        let root = polydat::dsl::compile_polydat(
            "shared has_a := true\n\
             shared has_b := false\n\
             selector := mod(cycle, 1)\n",
        )
        .expect("workload root compile");

        // Workload-root contract: has_a is Bool ExternalWrite slot
        // (SRD-13c §"Shared Mutable" step 1).
        let root_has_a_idx = root
            .program()
            .find_input("has_a")
            .expect("workload root has_a slot");
        let root_has_a_type = root.program().input_port_type("has_a");
        assert_eq!(
            root_has_a_type,
            Some(polydat::ast::PortType::Bool),
            "workload root has_a must be Bool"
        );
        let root_has_a_kind = root.program().input_kind(root_has_a_idx);
        assert_ne!(
            root_has_a_kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "workload root has_a must NOT be Coordinate; got {root_has_a_kind:?}"
        );

        // Step 2: for_each scope.
        let for_each = crate::scope_synth::build_for_each_scope_kernel(
            &[("outer".to_string(), "p1,p2".to_string())],
            &extract_manifest(root.program()),
            &root,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_for_each",
            None,
        )
        .expect("for_each synth");

        let fe_has_a_idx = for_each
            .program()
            .find_input("has_a")
            .expect("for_each has_a slot");
        let fe_has_a_type = for_each.program().input_port_type("has_a");
        assert_eq!(
            fe_has_a_type,
            Some(polydat::ast::PortType::Bool),
            "for_each has_a must be Bool, got {fe_has_a_type:?}"
        );
        let fe_has_a_kind = for_each.program().input_kind(fe_has_a_idx);
        assert_ne!(
            fe_has_a_kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "for_each has_a must NOT be Coordinate; got {fe_has_a_kind:?}"
        );

        // Step 3: phase scope with its own bindings via pick.
        let phase_bindings = nmbrs_workload::model::BindingsDef::PolydatSource(
            "chosen := pick(has_a, has_b, \"alpha\", \"beta\")\n".to_string(),
        );
        let phase = build_phase_scope_kernel(
            &phase_bindings,
            &[],
            &for_each,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_phase",
        )
        .expect("phase scope synth");

        let phase_has_a_idx = phase
            .program()
            .find_input("has_a")
            .expect("phase has_a slot");
        let phase_has_a_type = phase.program().input_port_type("has_a");
        assert_eq!(
            phase_has_a_type,
            Some(polydat::ast::PortType::Bool),
            "phase has_a must be Bool; got {phase_has_a_type:?}\n\
             phase input names: {:?}",
            phase.program().input_names()
        );
        let phase_has_a_kind = phase.program().input_kind(phase_has_a_idx);
        assert_ne!(
            phase_has_a_kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "phase has_a must NOT be Coordinate; got {phase_has_a_kind:?}\n\
             phase input names: {:?}\n\
             coord_count: {}",
            phase.program().input_names(),
            phase.program().coord_count()
        );
    }

    #[test]
    fn for_each_then_phase_preserves_shared_bool_through_chain() {
        // The full chain the workload exercises:
        //   workload root (shared has_X := false)
        //     → for_each scope (cascades has_X via synthesize_for_each_scope)
        //       → phase scope await_index (cascades has_X via build_phase_scope_kernel)
        //
        // SRD-13c §"Shared Mutable" + SRD-13d §1: at every cascade
        // hop, has_X stays Bool-typed and non-Coordinate.
        // bind_outer_scope then cell-attaches at each level so
        // the original SharedCell reaches the leaf.
        use polydat::kernel::extract_manifest;
        let root = polydat::dsl::compile_polydat(
            "input cycle: u64\nshared has_sai_column_indexes := false\n\
             shared has_indexes := false\n",
        )
        .expect("root compile");

        // for_each scope synthesised via the comprehension
        // synthesizer (the path runner.rs uses for ForComprehension
        // install specs). Iter vars are a placeholder so the
        // builder runs.
        let for_each_kernel = crate::scope_synth::build_for_each_scope_kernel(
            &[("dummy_var".to_string(), "1,2".to_string())],
            &extract_manifest(root.program()),
            &root,
            &HashMap::new(), // workload_params
            Vec::new(),      // polydat_lib_paths
            None,            // workload_dir
            false,           // strict
            "test_for_each",
            None, // phase_bindings
        )
        .expect("for_each kernel synth");

        // for_each's program must carry has_X as a Bool slot too.
        let fe_type = for_each_kernel
            .program()
            .input_port_type("has_sai_column_indexes");
        assert_eq!(
            fe_type,
            Some(polydat::ast::PortType::Bool),
            "for_each scope's has_sai_column_indexes must be Bool, got {fe_type:?}"
        );
        let fe_idx = for_each_kernel
            .program()
            .find_input("has_sai_column_indexes")
            .expect("for_each has has_sai_column_indexes input");
        let fe_kind = for_each_kernel.program().input_kind(fe_idx);
        assert_ne!(
            fe_kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "for_each scope's has_sai_column_indexes must NOT be Coordinate; got {fe_kind:?}"
        );

        // Now build await_index's phase scope under for_each
        // (the real scope-tree shape).
        let phase_bindings = nmbrs_workload::model::BindingsDef::PolydatSource(
            "target_index_table := pick(has_sai_column_indexes, has_indexes, \
             \"a\", \"b\")\n"
                .to_string(),
        );
        let phase_kernel = build_phase_scope_kernel(
            &phase_bindings,
            &[],
            &for_each_kernel,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_await_index",
        )
        .expect("phase kernel synth");

        let phase_type = phase_kernel
            .program()
            .input_port_type("has_sai_column_indexes");
        assert_eq!(
            phase_type,
            Some(polydat::ast::PortType::Bool),
            "phase scope's has_sai_column_indexes must be Bool, got {phase_type:?}"
        );
        let phase_idx = phase_kernel
            .program()
            .find_input("has_sai_column_indexes")
            .expect("phase has has_sai_column_indexes input");
        let phase_kind = phase_kernel.program().input_kind(phase_idx);
        assert_ne!(
            phase_kind,
            Some(polydat::kernel::InputKind::Coordinate),
            "phase scope's has_sai_column_indexes must NOT be Coordinate; got {phase_kind:?}"
        );
    }
}
