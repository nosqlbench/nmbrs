// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Bind point detection and field classification.
//!
//! Scans op template fields to detect `{name}` bind point references
//! and `{{expr}}` inline binding definitions. Classifies fields as
//! static (no bind points) or dynamic (has bind points).

use polydat::ast::PortType;

/// Namespace qualifier for a bind point reference.
///
/// Each namespace has one qualifier word, written lower-case:
/// `input`, `bind`, `capture`.
#[derive(Debug, Clone, PartialEq)]
pub enum BindQualifier {
    /// No qualifier — resolved by priority: bind → capture → input.
    None,
    /// `{input:name}` — graph input value.
    Input,
    /// `{bind:name}` — Polydat binding output.
    Bind,
    /// `{capture:name}` — capture context (volatile or sticky port).
    Capture,
}

impl BindQualifier {
    /// The qualifier a word names: `input`, `bind` or `capture`.
    /// `None` for any other word.
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "input" => Some(Self::Input),
            "bind" => Some(Self::Bind),
            "capture" => Some(Self::Capture),
            _ => None,
        }
    }

    /// The qualifier word to write in place of `word`, when `word`
    /// is a spelling that is not one: `coord` and `coordinate` (now
    /// `input`), `port` (now `capture`), and any other casing of a
    /// qualifier word (`Bind`). `None` for any other word,
    /// qualifier words included.
    fn retired_word(word: &str) -> Option<&'static str> {
        if Self::from_word(word).is_some() {
            return None;
        }
        match word.to_ascii_lowercase().as_str() {
            "input" | "coord" | "coordinate" => Some("input"),
            "bind" => Some("bind"),
            "capture" | "port" => Some("capture"),
            _ => None,
        }
    }
}

/// Workload-author lvalue assertion attached to a [`BindPoint::Reference`].
///
/// Spelled as a `:<spec>` suffix inside the brace pair:
///
/// - `{meta}` — no marker; strict match. The adapter consults
///   cluster-side metadata and the binder verifies the wire's
///   rvalue type matches that.
/// - `{meta:*}` — wildcard. Polydat is licensed to fuse / coerce;
///   the binder treats the slot as `Str`-lvalue (which permits any
///   rvalue per the load-bearing rule).
/// - `{meta:<type>}` — explicit polydat `PortType` assertion. The
///   binder uses the asserted type as the lvalue for verification,
///   overriding what cluster-side metadata would say. The type
///   name is the type's one polydat keyword (`u64`, `i32`, `f64`,
///   `vec_f32`, `str`, `bytes`, etc.) — the spelling `PortType`'s
///   `Display` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LvalueSpec {
    /// `:*` — workload-author opt-in to type fusion.
    Wildcard,
    /// `:<typename>` — workload-author asserts a polydat
    /// `PortType` by its keyword. Resolved here, through
    /// `PortType::from_workload_name`, so an adapter reads the
    /// asserted type and never parses a type name itself.
    Explicit(PortType),
}

/// A detected bind point in an op template field.
#[derive(Debug, Clone, PartialEq)]
pub enum BindPoint {
    /// `{name}`, `{qualifier:name}`, `{name:*}`, or `{name:<type>}` —
    /// references a named value with an optional lvalue assertion.
    ///
    /// `lvalue_spec` is the workload-author opt-in for the
    /// construction-time binder check. `None` (the default) means
    /// strict matching against cluster-side metadata. See
    /// [`LvalueSpec`] for the two relaxation forms.
    Reference {
        name: String,
        qualifier: BindQualifier,
        lvalue_spec: Option<LvalueSpec>,
    },
    /// `{{expr}}` — inline binding definition.
    InlineDefinition(String),
}

/// The classification of an op template field value.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    /// No bind points — value is constant across cycles.
    Static,
    /// Pure binding reference — the entire value is `{name}`.
    BindRef(String),
    /// String template with interleaved literals and bind points.
    Template(Vec<BindPoint>),
}

/// Scan a string value for bind points and classify it. Fails as
/// [`extract_bind_points`] does.
pub fn classify_field(value: &str) -> Result<FieldType, String> {
    let bind_points = extract_bind_points(value)?;
    Ok(if bind_points.is_empty() {
        FieldType::Static
    } else if bind_points.len() == 1
        && value.starts_with('{')
        && !value.starts_with("{{")
        && value.ends_with('}')
    {
        match &bind_points[0] {
            BindPoint::Reference { name, .. } => FieldType::BindRef(name.clone()),
            _ => FieldType::Template(bind_points),
        }
    } else {
        FieldType::Template(bind_points)
    })
}

/// Extract all bind points from a string.
///
/// Fails when a bind point is malformed: a `{name:<type>}` lvalue
/// assertion whose type is a spelling polydat retired or a type a
/// workload may not assert (see [`split_lvalue_spec`]). The error
/// quotes the bind point; a caller that knows the op and field
/// prefixes them.
pub fn extract_bind_points(value: &str) -> Result<Vec<BindPoint>, String> {
    let mut points = Vec::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                // Inline definition: {{expr}}
                i += 2;
                let start = i;
                while i + 1 < chars.len() && !(chars[i] == '}' && chars[i + 1] == '}') {
                    i += 1;
                }
                if i + 1 < chars.len() {
                    let expr: String = chars[start..i].iter().collect();
                    points.push(BindPoint::InlineDefinition(expr.trim().to_string()));
                    i += 2; // skip }}
                }
            } else {
                // Single brace: {name}, {:=expr}, {:=expr:=}, or {expr}
                // First, peek ahead to check if this is a CQL map literal
                // (starts with ' or ", possibly after whitespace — covers
                // multi-line CQL maps where the opening `{` sits on its
                // own line). If so, skip just the opening brace and
                // continue scanning — inner {name} refs are still valid.
                let next_nonspace = chars[i + 1..].iter().find(|c| !c.is_whitespace()).copied();
                if matches!(next_nonspace, Some(c) if is_literal_start(c)) {
                    // CQL map literal: {'key': '{value}'} — skip the opening {
                    // but continue scanning so inner bind points are found.
                    i += 1;
                    continue;
                }

                i += 1;
                let start = i;
                let mut depth = 1u32;
                while i < chars.len() {
                    if chars[i] == '{' {
                        depth += 1;
                    }
                    if chars[i] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                if i < chars.len() {
                    let raw: String = chars[start..i].iter().collect();
                    let raw = raw.trim();

                    if is_literal_content(raw) {
                        // Fallback: content has quotes — literal text, not a bind point.
                        i += 1;
                    } else if let Some(expr) = raw.strip_prefix(":=") {
                        // Explicit {:=expr} or {:=expr:=} syntax
                        let expr = expr.strip_suffix(":=").unwrap_or(expr).trim();
                        points.push(BindPoint::InlineDefinition(expr.to_string()));
                        i += 1;
                    } else if let Some((name_part, spec)) = extract_lvalue_spec(raw)? {
                        // Lvalue-asserted reference: `{name:*}` (wildcard)
                        // or `{name:<typename>}` (explicit polydat type).
                        // Detected BEFORE `is_expression` so the `:*` /
                        // `:type` suffix doesn't get misclassified as
                        // an operator. The body still routes through
                        // `parse_qualified_ref` for completeness, though
                        // qualifier+spec composition is not currently
                        // supported (the lvalue-spec detector only
                        // matches when `name_part` is a bare identifier).
                        let (qualifier, name) = parse_qualified_ref(name_part)?;
                        points.push(BindPoint::Reference {
                            name,
                            qualifier,
                            lvalue_spec: Some(spec),
                        });
                        i += 1;
                    } else if is_expression(raw) {
                        // Content has operators/parens — treat as inline expression
                        points.push(BindPoint::InlineDefinition(raw.to_string()));
                        i += 1;
                    } else {
                        // Simple identifier — reference bind point
                        let (qualifier, name) = parse_qualified_ref(raw)?;
                        points.push(BindPoint::Reference {
                            name,
                            qualifier,
                            lvalue_spec: None,
                        });
                        i += 1;
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    Ok(points)
}

/// Detect whether bind point content is a Polydat expression (not a simple name).
///
/// Returns true if the content contains operators, function calls,
/// or other syntax that can't be a plain identifier.
pub fn is_expression_public(s: &str) -> bool {
    is_expression(s)
}

/// Public form of `extract_lvalue_spec` for adapters that need
/// to interleave bind-point processing with text generation
/// (e.g., the cassandra-cpp `resolve_structural_and_mark_remaining`
/// walker, which scans the statement text char-by-char and must
/// strip a lvalue-spec suffix from a bind-point body before
/// deciding whether to inline-resolve or `?`-mark the position).
///
/// Returns `(bare_name, Some(spec))` when the body carries a
/// lvalue-spec suffix; `(body, None)` otherwise. The first half
/// of the tuple is always the body with any `:*` or `:<type>`
/// suffix stripped.
///
/// Fails when the suffix names a type but not one a workload may
/// assert: a spelling polydat retired (`{id:String}`: "`String` is
/// no longer a type keyword; write `str`") or an internal-only type
/// (`handle`, `ext`).
pub fn split_lvalue_spec(body: &str) -> Result<(&str, Option<LvalueSpec>), String> {
    Ok(match extract_lvalue_spec(body)? {
        Some((name_part, spec)) => (name_part, Some(spec)),
        None => (body, None),
    })
}

/// Detect the `:<spec>` lvalue-assertion suffix on a bind-point
/// body. Returns `Some((name_part, spec))` when the body is the
/// shape `<bare-identifier>:<*-or-type-keyword>`; `None` when it is
/// not an lvalue assertion at all; and an error when the suffix is
/// a type name that cannot be asserted.
///
/// Matched explicitly (not as part of `is_expression` or the
/// general qualifier-parse path) because:
///
/// - `:*` would otherwise trip the `*` arm of `is_expression` and
///   get treated as multiplication.
/// - `:<typename>` would otherwise get treated as
///   `<qualifier>:<name>` and the `name` could collide with a
///   real workload-named wire if both parts looked like
///   identifiers (e.g. `{input:cycle}` — qualifier `input`, name
///   `cycle` — must continue to parse as qualifier+name, not as
///   `name=input, lvalue=cycle`).
///
/// Both parts of the split are required to be bare identifiers
/// for the suffix detection to fire. That's how `{input:cycle}`
/// (qualifier path) is distinguished from `{meta:i32}` (lvalue-
/// spec path): the name part `meta` is a bare identifier, the
/// spec part `i32` is a bare identifier — but the LEFT side of
/// `input:cycle` matches a known qualifier word, so the existing
/// qualifier parser claims it first (in the outer dispatch this
/// helper runs from). Within this helper we don't even check
/// qualifier-ness — we just require both halves to be bare
/// identifier shape.
fn extract_lvalue_spec(raw: &str) -> Result<Option<(&str, LvalueSpec)>, String> {
    let Some((name_part, spec_part)) = raw.rsplit_once(':') else {
        return Ok(None);
    };
    let name_part = name_part.trim();
    let spec_part = spec_part.trim();
    if !is_bare_identifier(name_part) {
        return Ok(None);
    }
    // If the left side is a qualifier (`input`, `bind`,
    // `capture`), let the qualifier path claim this binding
    // instead — don't shadow it here. Composition
    // (`{capture:meta:type}`) is not currently supported; punt on
    // it cleanly by declining.
    if BindQualifier::from_word(name_part).is_some() {
        return Ok(None);
    }
    if spec_part == "*" {
        Ok(Some((name_part, LvalueSpec::Wildcard)))
    } else if let Some(port_type) = PortType::from_workload_name(spec_part) {
        Ok(Some((name_part, LvalueSpec::Explicit(port_type))))
    } else if let Some(retired) = PortType::retired_keyword_message(spec_part) {
        Err(format!("bind point `{{{raw}}}`: {retired}"))
    } else if PortType::from_keyword(spec_part).is_some() {
        Err(format!(
            "bind point `{{{raw}}}`: `{spec_part}` is an internal type; \
             a bind point cannot assert it"
        ))
    } else {
        // Suffix isn't `*` or a type name; this is not a
        // lvalue-spec binding. Fall through so the outer dispatch
        // lets the reference path claim the whole body.
        Ok(None)
    }
}

/// Bare-identifier shape: ASCII alpha or underscore start; rest
/// alphanumeric / underscore. Used by [`extract_lvalue_spec`] to
/// gate the suffix detection on shapes that look like names /
/// type-words and nothing else, and by the `set:` classifier
/// (SRD-18f §6) to decide a bare value is a wire reference.
pub(crate) fn is_bare_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_expression(s: &str) -> bool {
    // Simple identifiers: [a-zA-Z_][a-zA-Z0-9_-]*
    // Hyphens are valid in identifiers (e.g. my-variable), so only treat
    // `-` as an expression indicator when it is a unary minus (first char
    // followed by a digit) which marks a negative numeric literal.
    s.contains('(') || s.contains(')') ||
    s.contains('+') || s.contains('*') || s.contains('/') ||
    s.contains('%') || s.contains('^') || s.contains('&') ||
    s.contains('|') || s.contains('!') || s.contains('<') ||
    s.contains('>') ||
    // Numeric literal (starts with digit)
    s.starts_with(|c: char| c.is_ascii_digit()) ||
    // Negative literal: starts with - followed by digit
    (s.starts_with('-') && s.len() > 1 && s.as_bytes()[1].is_ascii_digit())
}

/// Content between `{` and `}` that is clearly literal text — not a
/// binding name or Polydat expression. If the content starts with a quote
/// character, it's a literal value (e.g. CQL map `{'class': ...}`),
/// never a bind point or expression.
fn is_literal_content(s: &str) -> bool {
    s.starts_with('\'') || s.starts_with('"')
}

/// Check if a character indicates the start of a CQL map/JSON literal
/// after an opening brace. `{'key': ...}` and `{"key": ...}` are
/// literal map content, not bind points.
fn is_literal_start(c: char) -> bool {
    c == '\'' || c == '"'
}

/// Parse a qualified reference like "input:cycle" or just "cycle".
///
/// A body whose prefix is not a qualifier word stays one
/// unqualified name. A prefix that is a retired spelling of a
/// qualifier, before a plain name, is refused with the bind point
/// to write: "`port` is not a bind-point qualifier; write
/// `{capture:auth_token}`".
fn parse_qualified_ref(raw: &str) -> Result<(BindQualifier, String), String> {
    let Some((prefix, name)) = raw.split_once(':') else {
        return Ok((BindQualifier::None, raw.to_string()));
    };
    let (prefix, name) = (prefix.trim(), name.trim());
    if let Some(qualifier) = BindQualifier::from_word(prefix) {
        return Ok((qualifier, name.to_string()));
    }
    if let Some(canonical) = BindQualifier::retired_word(prefix)
        && is_bare_identifier(name)
    {
        return Err(format!(
            "bind point `{{{raw}}}`: `{prefix}` is not a bind-point qualifier; \
             write `{{{canonical}:{name}}}`"
        ));
    }
    Ok((BindQualifier::None, raw.to_string()))
}

/// Extract all referenced binding names from a string (only `{name}`, not `{{expr}}`).
/// Returns the bare name without qualifier. Fails as
/// [`extract_bind_points`] does.
pub fn referenced_bindings(value: &str) -> Result<Vec<String>, String> {
    Ok(extract_bind_points(value)?
        .into_iter()
        .filter_map(|bp| match bp {
            BindPoint::Reference { name, .. } => Some(name),
            _ => None,
        })
        .collect())
}

/// Replace `{name}` bind point references with `?` markers for
/// CQL prepared statements. Returns the parameterized statement.
///
/// Also strips quotes that immediately surround a bind point:
/// `'{id}'` → `?` (not `'?'`), because CQL prepared bind markers
/// must not be inside string literals. Fails as
/// [`extract_bind_points`] does.
pub fn replace_bind_points_with_markers(value: &str) -> Result<String, String> {
    let names = referenced_bindings(value)?;
    let mut result = value.to_string();
    for name in &names {
        // Try quoted form first: '{name}' → ?
        let quoted = format!("'{{{name}}}'");
        if let Some(pos) = result.find(&quoted) {
            result.replace_range(pos..pos + quoted.len(), "?");
            continue;
        }
        // Bare form: {name} → ?
        let bare = format!("{{{name}}}");
        if let Some(pos) = result.find(&bare) {
            result.replace_range(pos..pos + bare.len(), "?");
        }
    }
    Ok(result)
}

// =================================================================
// Capture points: [name] and [name as alias] in op template strings
// =================================================================

/// Row-set aggregation for a declarative capture: reduce an
/// array-of-rows body (or addressed sub-array) to one scalar by
/// folding the named field across rows. Declared with a
/// `:min(field)` / `:max(field)` / `:sum(field)` suffix on the
/// capture path. Motivating case: `system_views.sstable_tasks`
/// carries several concurrent tasks in MIXED units (a
/// byte-denominated data compaction plus an ordinal-denominated
/// index build) — a `/0/...` capture pins whichever row sorts
/// first, so a saturated byte task masks the still-running merge;
/// `:min(completion_ratio)` reads the least-complete task, which
/// is the drain's true state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CaptureAgg {
    /// Numeric minimum of `field` across rows.
    Min(String),
    /// Numeric maximum of `field` across rows.
    Max(String),
    /// Numeric sum of `field` across rows.
    Sum(String),
}

/// A capture point extracted from an op template.
///
/// Capture points mark result fields that should be extracted from
/// the operation result and stored as named variables for use in
/// subsequent operations or verification.
///
/// Formats:
/// - `[username]` — capture "username" as "username" (single value)
/// - `[username as u1]` — capture "username", store as "u1" (single value)
/// - `[(List) field]` — capture with type assertion
/// - `[*]` — capture all available fields
/// - `[@keys]` — **slurp**: collect every row's `keys` column into
///   a `Value::Json` array. Use when the result has multiple rows
///   and the consumer needs all per-row column values as a list
///   (e.g. recall-evaluator's `actual:` reads).
/// - `[@col as values]` — slurp with alias.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapturePoint {
    /// The field name to capture from the result.
    pub source_name: String,
    /// The variable name to store the captured value under.
    /// Same as source_name if no `as` clause.
    pub as_name: String,
    /// Optional type assertion (e.g., "List", "int[]").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast_type: Option<String>,
    /// `true` when the capture-point was declared with the `@`
    /// slurp prefix (`[@name]`). Slurp captures collect every
    /// row's column value across the result body into a single
    /// `Value::Json` array; non-slurp captures take the first
    /// row's value as a scalar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub slurp: bool,
    /// Optional JSON-Pointer path (RFC 6901, e.g. `/0/value`,
    /// `/value/inner/0`). When `Some`, the extractor uses
    /// `serde_json::Value::pointer(path)` against the body's
    /// `to_json()` projection rather than the first-row /
    /// top-level field lookup keyed by `source_name`. Lets a
    /// declarative `capture:` map address Jolokia bulk-POST
    /// responses (`[{value:N}, {value:[...]}, ...]`) by
    /// position + nested field.
    ///
    /// The bracket-syntax in op text (e.g. `[name]`) leaves
    /// this `None` and continues to use the original first-row
    /// extraction path; only the declarative `capture:` block
    /// populates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Reduce the extracted sub-tree to a u64 count instead of
    /// capturing it as-is. Array → length; object → key count;
    /// scalar (number / bool / non-empty string) → 1; null /
    /// missing → 0. Useful for "is the compactions list empty"
    /// style predicates without binding the array into the
    /// kernel as a `Value::Json`. Only honoured when `path` is
    /// also set (declarative capture form).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub count: bool,
    /// Row-set aggregation (`:min(f)` / `:max(f)` / `:sum(f)`
    /// path suffix): fold the named field across the rows of the
    /// addressed array instead of capturing a single value. Like
    /// `count`, only honoured on the declarative capture form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agg: Option<CaptureAgg>,
    /// Row predicate for an aggregate: `(field, value)`, from the
    /// `where <field>='<value>'` clause of a capture suffix — e.g.
    /// `":sum(progress where kind='secondary index build')"`. Rows whose
    /// `field` does not equal `value` are excluded before folding.
    ///
    /// Exists because a result set can hold rows that are not commensurable.
    /// `system_views.sstable_tasks` lists a data compaction reporting
    /// `unit=bytes` beside a vector index build reporting
    /// `unit=token range parts`; summing across them adds unlike quantities,
    /// and the total silently depends on how many tasks were registered at
    /// sample time. The predicate cannot live in the query — Cassandra allows
    /// `GROUP BY` only on PRIMARY KEY columns, and narrowing the poll's own
    /// `WHERE` would change which rows the drain waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_filter: Option<(String, String)>,
}

/// Result of parsing capture points from a string.
#[derive(Debug, Clone)]
pub struct CaptureParseResult {
    /// The raw template with capture brackets removed.
    /// `select [username] from t` → `select username from t`
    pub raw_template: String,
    /// The capture points found.
    pub captures: Vec<CapturePoint>,
}

/// Parse capture points from an op template string.
///
/// Detects `[name]`, `[name as alias]`, `[(Type) name]`, and `[*]`
/// patterns. Returns the cleaned template (brackets removed) and
/// the list of capture points.
pub fn parse_capture_points(template: &str) -> CaptureParseResult {
    let mut captures = Vec::new();
    let mut raw = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '[' {
            let bracket_start = i;
            let attempt_start = i + 1;
            i = attempt_start;

            // Speculatively parse a capture-point spec. If anything
            // about the grammar fails to match — including the
            // source name being absent or not starting with an
            // identifier character — we reset to `bracket_start + 1`
            // and emit the `[` as a literal. This keeps JSON / CQL
            // array literals (`[]`, `["foo", 42]`), JNI type
            // signatures (`[Ljava.lang.String;`), and other uses of
            // `[...]` from being silently consumed.

            // Skip whitespace
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }

            // Optional type cast: (Type)
            let cast_type = if i < chars.len() && chars[i] == '(' {
                i += 1;
                let cast_start = i;
                while i < chars.len() && chars[i] != ')' {
                    i += 1;
                }
                let cast: String = chars[cast_start..i].iter().collect();
                if i < chars.len() {
                    i += 1;
                } // skip ')'
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                Some(cast.trim().to_string())
            } else {
                None
            };

            // Optional slurp modifier: `[@name]` collects every
            // row's column value into a `Value::Json` array. The
            // `@` is a syntax-only marker — stripped from the
            // emitted raw_template so the adapter sees clean
            // column-reference text.
            let slurp = if i < chars.len() && chars[i] == '@' {
                i += 1;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                true
            } else {
                false
            };

            // Source name — must look like a real identifier (or
            // `*` for wildcard). Starts with `_` or an ASCII alpha
            // character; continues with alphanumerics, `_`, `-`,
            // `.`, or `*`. The strict-leading-char rule prevents
            // JSON array literals like `["..."]`, `[42]`, and JNI
            // signatures like `[Ljava.lang.String;` from being
            // misread as captures.
            let name_start = i;
            if i < chars.len() {
                let first = chars[i];
                let is_valid_first = first.is_ascii_alphabetic() || first == '_' || first == '*';
                if !is_valid_first {
                    // Not a capture-point opening — emit `[` and
                    // resume one char in.
                    raw.push('[');
                    i = attempt_start;
                    continue;
                }
            }
            while i < chars.len()
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '-'
                    || chars[i] == '.'
                    || chars[i] == '*')
            {
                i += 1;
            }
            let source_name: String = chars[name_start..i].iter().collect();
            if source_name.is_empty() {
                // Defense-in-depth — the leading-char check above
                // already excludes this path, but keep it explicit.
                raw.push('[');
                i = attempt_start;
                continue;
            }

            // Optional "as alias"
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let as_name = if i + 2 < chars.len()
                && (chars[i] == 'a' || chars[i] == 'A')
                && (chars[i + 1] == 's' || chars[i + 1] == 'S')
                && chars[i + 2].is_whitespace()
            {
                i += 2; // skip "as"
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                let alias_start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric()
                        || chars[i] == '_'
                        || chars[i] == '-'
                        || chars[i] == '.')
                {
                    i += 1;
                }
                let alias: String = chars[alias_start..i].iter().collect();
                alias
            } else {
                source_name.clone()
            };

            // Skip whitespace and closing bracket
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                i += 1;
                captures.push(CapturePoint {
                    // Bracket syntax carries no aggregate, so no predicate.
                    row_filter: None,
                    source_name: source_name.clone(),
                    as_name,
                    cast_type,
                    slurp,
                    path: None,
                    count: false,
                    agg: None,
                });
                // Emit source name without brackets into raw template
                raw.push_str(&source_name);
            } else {
                // No matching `]` for what otherwise looked like a
                // capture spec — likely a CQL/JSON array containing
                // an identifier (e.g. `[foo, bar]`). Treat the
                // opening `[` as a literal and resume one char in.
                let _ = bracket_start;
                raw.push('[');
                i = attempt_start;
            }
        } else {
            raw.push(chars[i]);
            i += 1;
        }
    }

    CaptureParseResult {
        raw_template: raw,
        captures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{name:<type>}` resolves the type through polydat's keyword
    /// table, so every keyword a workload author may assert is an
    /// lvalue spec, and the internal-only `handle` and `ext` are not.
    #[test]
    fn lvalue_spec_resolves_polydat_type_keywords() {
        for (body, expected) in [
            ("meta:i32", PortType::I32),
            ("id:str", PortType::Str),
            ("emb:vec_f32", PortType::VecF32),
            ("ids:vec_i64", PortType::VecI64),
        ] {
            let (name, spec) = split_lvalue_spec(body).unwrap();
            assert_eq!(spec, Some(LvalueSpec::Explicit(expected)), "{body}");
            assert_eq!(Some(name), body.split(':').next(), "{body}");
        }
        assert_eq!(
            split_lvalue_spec("meta:*").unwrap(),
            ("meta", Some(LvalueSpec::Wildcard))
        );
        assert_eq!(
            split_lvalue_spec("host:auth_token").unwrap(),
            ("host:auth_token", None)
        );
    }

    /// A suffix that names a type a workload may not assert is
    /// refused, quoting the bind point: a spelling polydat retired
    /// with the keyword to write, an internal-only type as such.
    #[test]
    fn lvalue_spec_refuses_a_type_that_cannot_be_asserted() {
        assert_eq!(
            extract_bind_points("WHERE id = {id:String}"),
            Err(
                "bind point `{id:String}`: `String` is no longer a type keyword; write `str`"
                    .to_string()
            )
        );
        for (body, typ) in [("h:handle", "handle"), ("p:ext", "ext")] {
            assert_eq!(
                split_lvalue_spec(body),
                Err(format!(
                    "bind point `{{{body}}}`: `{typ}` is an internal type; \
                     a bind point cannot assert it"
                )),
            );
        }
        assert!(referenced_bindings("{id:Json}").is_err());
        assert!(classify_field("{id:Str}").is_err());
        assert!(replace_bind_points_with_markers("{id:Ext}").is_err());
    }

    #[test]
    fn is_expression_detects_function_calls() {
        assert!(is_expression("hash(cycle)"));
        assert!(is_expression("mod(x, 100)"));
    }

    #[test]
    fn is_expression_detects_operators() {
        assert!(is_expression("x + 1"));
        assert!(is_expression("a * b"));
        assert!(is_expression("x & 0xFF"));
    }

    #[test]
    fn is_expression_rejects_simple_names() {
        assert!(!is_expression("cycle"));
        assert!(!is_expression("my_var"));
        assert!(!is_expression("user_id"));
    }

    #[test]
    fn is_expression_rejects_hyphenated_names() {
        assert!(!is_expression("my-variable"));
        assert!(!is_expression("some-long-name"));
    }

    #[test]
    fn is_expression_detects_numeric_literals() {
        assert!(is_expression("42"));
        assert!(is_expression("3.14"));
        assert!(is_expression("-5"));
    }

    #[test]
    fn static_field() {
        assert_eq!(classify_field("plain text").unwrap(), FieldType::Static);
        assert_eq!(classify_field("42").unwrap(), FieldType::Static);
        assert_eq!(classify_field("").unwrap(), FieldType::Static);
    }

    #[test]
    fn pure_bind_ref() {
        assert_eq!(
            classify_field("{userid}").unwrap(),
            FieldType::BindRef("userid".into())
        );
    }

    #[test]
    fn template_with_bind_points() {
        let ft = classify_field("SELECT * FROM t WHERE id={id} AND name={name}").unwrap();
        match ft {
            FieldType::Template(points) => {
                assert_eq!(points.len(), 2);
                assert_eq!(
                    points[0],
                    BindPoint::Reference {
                        name: "id".into(),
                        qualifier: BindQualifier::None,
                        lvalue_spec: None
                    }
                );
                assert_eq!(
                    points[1],
                    BindPoint::Reference {
                        name: "name".into(),
                        qualifier: BindQualifier::None,
                        lvalue_spec: None
                    }
                );
            }
            _ => panic!("expected Template"),
        }
    }

    #[test]
    fn inline_definition() {
        let ft = classify_field("value is {{Template('user-{}', ToString())}}").unwrap();
        match ft {
            FieldType::Template(points) => {
                assert_eq!(points.len(), 1);
                match &points[0] {
                    BindPoint::InlineDefinition(expr) => {
                        assert!(expr.contains("Template"));
                    }
                    _ => panic!("expected InlineDefinition"),
                }
            }
            _ => panic!("expected Template"),
        }
    }

    #[test]
    fn mixed_references_and_literals() {
        let refs = referenced_bindings("INSERT INTO t (a, b) VALUES ({col_a}, {col_b})").unwrap();
        assert_eq!(refs, vec!["col_a", "col_b"]);
    }

    #[test]
    fn no_bind_points() {
        let refs = referenced_bindings("just a plain string").unwrap();
        assert!(refs.is_empty());
    }

    // --- Qualified bind point tests ---

    #[test]
    fn qualified_input() {
        let points = extract_bind_points("{input:cycle}").unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0],
            BindPoint::Reference {
                name: "cycle".into(),
                qualifier: BindQualifier::Input,
                lvalue_spec: None,
            }
        );
    }

    #[test]
    fn qualified_capture() {
        let points = extract_bind_points("{capture:balance}").unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0],
            BindPoint::Reference {
                name: "balance".into(),
                qualifier: BindQualifier::Capture,
                lvalue_spec: None,
            }
        );
    }

    #[test]
    fn qualified_bind() {
        let points = extract_bind_points("{bind:user_id}").unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0],
            BindPoint::Reference {
                name: "user_id".into(),
                qualifier: BindQualifier::Bind,
                lvalue_spec: None,
            }
        );
    }

    #[test]
    fn unknown_qualifier_becomes_unqualified() {
        // "host" is not a qualifier — treated as unqualified
        let points = extract_bind_points("{host:auth_token}").unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0],
            BindPoint::Reference {
                name: "host:auth_token".into(),
                qualifier: BindQualifier::None,
                lvalue_spec: None,
            }
        );
    }

    #[test]
    fn unqualified_still_works() {
        let points = extract_bind_points("{user_id}").unwrap();
        assert_eq!(
            points[0],
            BindPoint::Reference {
                name: "user_id".into(),
                qualifier: BindQualifier::None,
                lvalue_spec: None,
            }
        );
    }

    #[test]
    fn qualified_referenced_bindings_returns_bare_name() {
        let refs =
            referenced_bindings("VALUES ({input:cycle}, {capture:balance}, {user_id})").unwrap();
        assert_eq!(refs, vec!["cycle", "balance", "user_id"]);
    }

    /// Each namespace has one qualifier word; a retired spelling of
    /// one is refused with the bind point to write.
    #[test]
    fn a_retired_qualifier_spelling_is_refused_naming_the_qualifier() {
        for (body, write) in [
            ("coord:cycle", "input:cycle"),
            ("coordinate:row", "input:row"),
            ("port:auth_token", "capture:auth_token"),
            ("Bind:user_id", "bind:user_id"),
        ] {
            let prefix = body.split(':').next().unwrap();
            assert_eq!(
                extract_bind_points(&format!("VALUES ({{{body}}})")),
                Err(format!(
                    "bind point `{{{body}}}`: `{prefix}` is not a bind-point qualifier; \
                     write `{{{write}}}`"
                )),
            );
        }
        // A wire that happens to be named like a retired qualifier
        // still takes an lvalue assertion.
        assert_eq!(
            split_lvalue_spec("port:u64").unwrap(),
            ("port", Some(LvalueSpec::Explicit(PortType::U64)))
        );
    }

    // --- Capture point tests ---

    #[test]
    fn capture_simple() {
        let result = parse_capture_points("select [username] from users where id={id}");
        assert_eq!(result.captures.len(), 1);
        assert_eq!(result.captures[0].source_name, "username");
        assert_eq!(result.captures[0].as_name, "username");
        assert_eq!(
            result.raw_template,
            "select username from users where id={id}"
        );
    }

    #[test]
    fn capture_with_alias() {
        let result = parse_capture_points("select [username as u1] from users");
        assert_eq!(result.captures.len(), 1);
        assert_eq!(result.captures[0].source_name, "username");
        assert_eq!(result.captures[0].as_name, "u1");
        assert_eq!(result.raw_template, "select username from users");
    }

    #[test]
    fn capture_with_type_cast() {
        let result = parse_capture_points("select [(List) items] from orders");
        assert_eq!(result.captures.len(), 1);
        assert_eq!(result.captures[0].source_name, "items");
        assert_eq!(result.captures[0].cast_type, Some("List".into()));
    }

    #[test]
    fn capture_wildcard() {
        let result = parse_capture_points("select [*] from users");
        assert_eq!(result.captures.len(), 1);
        assert_eq!(result.captures[0].source_name, "*");
    }

    #[test]
    fn capture_empty_brackets_pass_through() {
        // `[]` (empty JSON array literal) is NOT a capture point.
        // The parser must emit it verbatim — eating the brackets
        // would break Jolokia payloads like
        // `"arguments":["foo",[]]` which became `"arguments":["foo",]`
        // (invalid JSON, JMX op gets 1 arg instead of 2).
        let result = parse_capture_points(r#"{"arguments":["foo",[]]}"#);
        assert!(
            result.captures.is_empty(),
            "no capture should be extracted: {:?}",
            result.captures
        );
        assert_eq!(result.raw_template, r#"{"arguments":["foo",[]]}"#);
    }

    #[test]
    fn capture_jni_array_signature_pass_through() {
        // JNI array signature `[Ljava.lang.String;` opens with `[`
        // but is not a capture point (no closing `]` follows the
        // identifier chunk). The parser must not consume anything.
        let template = r#""operation":"forceKeyspaceFlush(java.lang.String,[Ljava.lang.String;)""#;
        let result = parse_capture_points(template);
        assert!(
            result.captures.is_empty(),
            "JNI signature should not parse as capture: {:?}",
            result.captures
        );
        assert_eq!(result.raw_template, template);
    }

    #[test]
    fn capture_json_array_with_string_pass_through() {
        // JSON string-array literal — opens `[`, content begins
        // with `"`, no identifier; parser must leave it alone.
        let result = parse_capture_points(r#"["alpha","beta"]"#);
        assert!(result.captures.is_empty());
        assert_eq!(result.raw_template, r#"["alpha","beta"]"#);
    }

    #[test]
    fn capture_json_array_with_number_pass_through() {
        // Numeric-literal-only array — leading digit is not a
        // valid identifier start, so no capture is extracted.
        let result = parse_capture_points("[42]");
        assert!(result.captures.is_empty());
        assert_eq!(result.raw_template, "[42]");
    }

    #[test]
    fn capture_cql_collection_literal_pass_through() {
        // CQL collection literal like `[1, 2, 3]` — content has
        // digits + commas, not a valid capture spec. Must
        // pass through verbatim.
        let template = "INSERT INTO t (vals) VALUES ([1, 2, 3])";
        let result = parse_capture_points(template);
        assert!(result.captures.is_empty());
        assert_eq!(result.raw_template, template);
    }

    #[test]
    fn capture_multiple() {
        let result = parse_capture_points("select [a], [b as x] from t where id={id}");
        assert_eq!(result.captures.len(), 2);
        assert_eq!(result.captures[0].source_name, "a");
        assert_eq!(result.captures[0].as_name, "a");
        assert_eq!(result.captures[1].source_name, "b");
        assert_eq!(result.captures[1].as_name, "x");
    }

    #[test]
    fn capture_no_captures() {
        let result = parse_capture_points("select * from users where id={id}");
        assert!(result.captures.is_empty());
        assert_eq!(result.raw_template, "select * from users where id={id}");
    }

    #[test]
    fn capture_mixed_with_bind_points() {
        let result =
            parse_capture_points("select [name], [age as user_age] from users where id={userid}");
        assert_eq!(result.captures.len(), 2);
        // Bind point {userid} should remain in the raw template
        assert!(result.raw_template.contains("{userid}"));
        // Capture brackets should be removed
        assert!(!result.raw_template.contains('['));
        assert!(!result.raw_template.contains(']'));
    }
}
