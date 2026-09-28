// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Gutter wrapper — publishes the phase's contextual left-gutter
//! cell from workload-declared polydat templates.
//!
//! Distinct from the `memo` wrapper: memo owns the `[[ ... ]]`
//! header line above the phase status; the gutter owns the compact
//! cell to the LEFT of the phase's detail row, where the display
//! otherwise auto-derives a completion bar (metered phases) or a
//! latency trend (daemons). This wrapper lets the workload take
//! that cell over with its own computed value.
//!
//! Declaration forms:
//!
//! - `gutter: "<layout template>"` — printf-style polydat layout
//!   string; the rendered text fills the cell verbatim.
//! - `gutter: { bar: "<template>" }` — template renders to an
//!   `f64` fraction in `0..=1`; displayed as the house braille
//!   completion bar.
//! - `gutter: { spark: "<template>" }` — template renders to an
//!   `f64` sample; each publication appends to a per-phase trend
//!   ring displayed as a sparkline with the current value.
//! - `gutter: { …, final: <string | {bar|spark|text}> }` — the
//!   COMPLETION form: evaluated once at phase end and rendered as
//!   the left-gutter cell of the phase's ✓ outcome DETAIL line
//!   (the header line's timing triad is never touched). Final
//!   templates may also reference status-metric names (`{recall}`,
//!   `{latency_p50}`, …) — resolved from the phase's aggregates
//!   when no wire matches. Phases with a during-form but no
//!   `final:` still get ONE final update: the during template
//!   re-evaluated at phase end.
//!
//! Inside a `poll:` drain, the during form is additionally
//! re-published per poll iteration by the poll wrapper (against
//! that iteration's captures, like the poll memo) — this wrapper
//! alone fires only when the drain op completes.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

pub const NAME: WrapperName = WrapperName::new("gutter");

/// One published gutter-cell value. The display consumes this via
/// the phase render handle; `None` in the activity's slot means
/// "derive automatically".
#[derive(Debug, Clone, PartialEq)]
pub enum GutterSpec {
    /// Pre-rendered layout text, placed in the cell verbatim
    /// (truncated to the cell width by the display).
    Text(String),
    /// Completion fraction `0..=1` → house braille bar.
    Bar(f64),
    /// One trend sample → sparkline ring + current-value label.
    Spark(f64),
    /// A key metric: label and value rendered on opposite sides of the cell.
    /// Declared as `gutter: { final: { name: "recall", value: "{recall}" } }`.
    /// The pair is explicit rather than split out of one string because the two
    /// orders both occur in practice (`recall 96%` vs `≈900/s units`), so no
    /// splitting rule can infer which token is the value.
    Labeled { name: String, value: String },
}

/// Trigger: `gutter:` is a string (layout template) or a map with
/// a `bar:` or `spark:` key.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template
        .params
        .get("gutter")
        .map(|v| v.is_string() || v.is_object())
        .unwrap_or(false)
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    let v = template.params.get("gutter")?;
    if let Some(s) = v.as_str() {
        if s.is_empty() {
            return None;
        }
        Some(format!("gutter: \"{s}\" (layout)"))
    } else if let Some(obj) = v.as_object() {
        let mut parts: Vec<String> = Vec::new();
        for key in ["bar", "spark", "text"] {
            if let Some(t) = obj.get(key).and_then(|x| x.as_str()) {
                parts.push(format!("{key} \"{t}\""));
            }
        }
        match obj.get("final") {
            Some(f) if f.is_string() => {
                parts.push(format!("final \"{}\"", f.as_str().unwrap_or("")))
            }
            Some(f) if f.is_object() => parts.push("final {…}".to_string()),
            _ => {}
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("gutter: {}", parts.join(" / ")))
        }
    } else {
        None
    }
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        // `gutter:` is the sole discriminant — layout string or
        // `{bar}` / `{spark}` map. Like memo, publication is
        // independent of every other wrapper's behaviour: it sees
        // the same wires and writes to its own atomic slot.
        owned_fields: &["gutter"],
        triggers,
        requires_inner: &[super::traverse::NAME],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// The parsed declaration — which cell shape the template feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterKind {
    Text,
    Bar,
    Spark,
    /// Key metric: label and value, carried through the template pipeline as a
    /// single string joined by US (unit separator) so interpolation of both
    /// halves happens in one pass, then split at render time.
    Labeled,
}

/// Parse an op's `gutter:` param value into its `(during, final)`
/// template forms. Shared by the wrapper-cascade arm (which builds
/// the [`GutterDispenser`] and stores the specs on the activity) and
/// the poll wrapper (which re-publishes the during form per poll
/// iteration so a single long drain op keeps a live cell).
pub fn parse_specs(
    v: Option<&serde_json::Value>,
) -> (Option<(GutterKind, String)>, Option<(GutterKind, String)>) {
    let parse_forms =
        |obj: &serde_json::Map<String, serde_json::Value>| -> Option<(GutterKind, String)> {
            if let Some(m) = obj.get("labeled").and_then(|x| x.as_object()) {
                let name = m
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let value = m
                    .get("value")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some((GutterKind::Labeled, format!("{name}{}{value}", '\u{1f}')));
            }
            if let Some(t) = obj.get("bar").and_then(|x| x.as_str()) {
                Some((GutterKind::Bar, t.to_string()))
            } else if let Some(t) = obj.get("spark").and_then(|x| x.as_str()) {
                Some((GutterKind::Spark, t.to_string()))
            } else {
                obj.get("text")
                    .and_then(|x| x.as_str())
                    .map(|t| (GutterKind::Text, t.to_string()))
            }
        };
    match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            (Some((GutterKind::Text, s.clone())), None)
        }
        Some(serde_json::Value::Object(obj)) => {
            let during = parse_forms(obj);
            let fin = match obj.get("final") {
                Some(serde_json::Value::String(s)) if !s.is_empty() => {
                    Some((GutterKind::Text, s.clone()))
                }
                Some(serde_json::Value::Object(fobj)) => parse_forms(fobj),
                _ => None,
            };
            (during, fin)
        }
        _ => (None, None),
    }
}

/// Render one gutter template against the wires into its publishable
/// spec. `None` on substitution failure or (for the numeric kinds) a
/// non-numeric render — callers leave the last good value in place.
pub(crate) fn render_spec(
    kind: GutterKind,
    template: &str,
    wires: &dyn crate::wires::WireSource,
) -> Option<GutterSpec> {
    let rendered = match crate::wires::substitute_via_wires(template, wires) {
        Ok(s) => s,
        Err(e) => {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "gutter: substitution failed for '{template}': {e}"
            );
            return None;
        }
    };
    match kind {
        GutterKind::Labeled => {
            let (name, value) = rendered
                .split_once('\u{1f}')
                .unwrap_or(("", rendered.as_str()));
            Some(GutterSpec::Labeled {
                name: name.to_string(),
                value: value.to_string(),
            })
        }
        GutterKind::Text => Some(GutterSpec::Text(rendered)),
        GutterKind::Bar | GutterKind::Spark => match rendered.trim().parse::<f64>() {
            Ok(v) if kind == GutterKind::Bar => Some(GutterSpec::Bar(v.clamp(0.0, 1.0))),
            Ok(v) => Some(GutterSpec::Spark(v)),
            Err(_) => {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "gutter: '{template}' rendered to non-numeric '{rendered}'"
                );
                None
            }
        },
    }
}

/// Op-wrapper publishing the phase's gutter-cell spec after each
/// successful inner op (post-op, so captures from THIS execution
/// are on the wires — the cell reflects measured state).
///
/// No-op on inner errors; substitution or parse failures degrade
/// to a debug log — the gutter must never fail an otherwise-good
/// op. Numeric kinds (`bar` / `spark`) additionally require the
/// rendered string to parse as `f64`.
pub struct GutterDispenser {
    inner: Arc<dyn OpDispenser>,
    kind: GutterKind,
    template: String,
    /// Shared slot owned by the activity (see `Activity::gutter`).
    /// Writes here are visible to the display fold without a
    /// separate channel.
    gutter_state: Arc<arc_swap::ArcSwapOption<GutterSpec>>,
}

impl GutterDispenser {
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        kind: GutterKind,
        template: String,
        gutter_state: Arc<arc_swap::ArcSwapOption<GutterSpec>>,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            kind,
            template,
            gutter_state,
        })
    }

    fn publish(&self, wires: &dyn crate::wires::WireSource) {
        if let Some(spec) = render_spec(self.kind, &self.template, wires) {
            self.gutter_state.store(Some(Arc::new(spec)));
        }
    }
}

impl WrappingDispenser for GutterDispenser {}

impl OpDispenser for GutterDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self.inner.execute(cycle, ctx).await?;
            self.publish(ctx.wires);
            Ok(result)
        })
    }

    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, ExecutionError, OpResult};
    use crate::fixture::{ExecCtx, ResolvedPulls};

    struct FakeInner {
        error: Option<&'static str>,
    }

    impl OpDispenser for FakeInner {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async move {
                if let Some(msg) = self.error {
                    return Err(ExecutionError::Op(AdapterError {
                        error_name: "test".into(),
                        message: msg.into(),
                        retryable: false,
                    }));
                }
                Ok(OpResult {
                    body: None,
                    skipped: false,
                })
            })
        }
    }

    fn empty_ctx() -> (crate::adapter::ResolvedFields, ResolvedPulls) {
        let fields = crate::adapter::ResolvedFields::new(vec![], vec![]);
        let pulls = ResolvedPulls::empty();
        (fields, pulls)
    }

    #[tokio::test]
    async fn text_form_publishes_rendered_layout() {
        let state = Arc::new(arc_swap::ArcSwapOption::empty());
        let inner = Arc::new(FakeInner { error: None });
        let d = GutterDispenser::wrap(
            inner,
            GutterKind::Text,
            "queue depth ok".into(),
            state.clone(),
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let _ = d.execute(0, &ctx).await.expect("inner ok");
        assert_eq!(
            state.load().as_deref(),
            Some(&GutterSpec::Text("queue depth ok".into()))
        );
    }

    #[tokio::test]
    async fn bar_form_parses_and_clamps_fraction() {
        let state = Arc::new(arc_swap::ArcSwapOption::empty());
        let inner = Arc::new(FakeInner { error: None });
        let d = GutterDispenser::wrap(inner, GutterKind::Bar, "1.7".into(), state.clone());
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let _ = d.execute(0, &ctx).await.expect("inner ok");
        assert_eq!(
            state.load().as_deref(),
            Some(&GutterSpec::Bar(1.0)),
            "fraction must clamp to 0..=1"
        );
    }

    #[tokio::test]
    async fn numeric_parse_failure_leaves_slot_untouched() {
        let state: Arc<arc_swap::ArcSwapOption<GutterSpec>> =
            Arc::new(arc_swap::ArcSwapOption::empty());
        state.store(Some(Arc::new(GutterSpec::Spark(3.0))));
        let inner = Arc::new(FakeInner { error: None });
        let d = GutterDispenser::wrap(
            inner,
            GutterKind::Spark,
            "not-a-number".into(),
            state.clone(),
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let _ = d.execute(0, &ctx).await.expect("inner ok");
        assert_eq!(
            state.load().as_deref(),
            Some(&GutterSpec::Spark(3.0)),
            "unparseable render must not clobber the last good value"
        );
    }

    #[tokio::test]
    async fn does_not_publish_on_inner_error() {
        let state: Arc<arc_swap::ArcSwapOption<GutterSpec>> =
            Arc::new(arc_swap::ArcSwapOption::empty());
        let inner = Arc::new(FakeInner {
            error: Some("boom"),
        });
        let d = GutterDispenser::wrap(inner, GutterKind::Text, "never".into(), state.clone());
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        assert!(d.execute(0, &ctx).await.is_err());
        assert!(
            state.load().is_none(),
            "gutter must not publish on inner error"
        );
    }
}
