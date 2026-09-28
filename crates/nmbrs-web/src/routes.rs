// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Route handlers for the web UI.
//!
//! Each page route checks the `HX-Request` header. When present (htmx
//! navigation), only the `<main>` content fragment is returned. When
//! absent (direct browser load), the full page with base shell is
//! returned. This avoids separate `/api/*` routes for navigation.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;

use nmbrs_metrics::reporters::openmetrics_parse;
use polydat::dsl::registry;
use polydat::viz;

use crate::models::*;
use crate::ws::MetricsBroadcast;

/// Returns `true` when the request comes from htmx (partial swap).
fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("HX-Request")
}

// ─── Dashboard ──────────────────────────────────────────────

pub async fn dashboard(headers: HeaderMap) -> Html<String> {
    let (tc, ops, p99, ec) = ("0".into(), "\u{2014}".into(), "\u{2014}".into(), "0".into());
    let activities = vec![];

    if is_htmx(&headers) {
        let frag = DashboardContentFragment {
            total_cycles: tc,
            ops_per_sec: ops,
            p99_ms: p99,
            error_count: ec,
            activities,
        };
        Html(frag.render().expect("dashboard content fragment"))
    } else {
        let page = DashboardPage {
            total_cycles: tc,
            ops_per_sec: ops,
            p99_ms: p99,
            error_count: ec,
            activities,
        };
        Html(page.render().expect("dashboard template"))
    }
}

// ─── Functions ──────────────────────────────────────────────

pub async fn functions_page(headers: HeaderMap) -> Html<String> {
    let groups = build_function_groups(None);
    if is_htmx(&headers) {
        let frag = FunctionsContentFragment { groups };
        Html(frag.render().expect("functions content fragment"))
    } else {
        let page = FunctionsPage { groups };
        Html(page.render().expect("functions template"))
    }
}

#[derive(serde::Deserialize)]
pub struct FunctionQuery {
    pub q: Option<String>,
}

pub async fn functions_api(Query(query): Query<FunctionQuery>) -> Html<String> {
    let groups = build_function_groups(query.q.as_deref());
    let fragment = FunctionTableFragment { groups };
    Html(fragment.render().expect("function_table fragment"))
}

// ─── Stdlib ─────────────────────────────────────────────────

pub async fn stdlib_page(headers: HeaderMap) -> Html<String> {
    let groups = build_stdlib_groups();
    if is_htmx(&headers) {
        let frag = StdlibContentFragment { groups };
        Html(frag.render().expect("stdlib content fragment"))
    } else {
        let page = StdlibPage { groups };
        Html(page.render().expect("stdlib template"))
    }
}

pub async fn stdlib_source(axum::extract::Path(name): axum::extract::Path<String>) -> Html<String> {
    let sources = polydat::dsl::stdlib_sources();
    for (_filename, source) in sources {
        if source.contains(&format!("{name}(")) {
            return Html(format!(
                "<pre style=\"margin-top: 8px;\">{}</pre>",
                esc(source)
            ));
        }
    }
    Html("<pre>Module not found</pre>".into())
}

// ─── DAG Viewer ─────────────────────────────────────────────

pub async fn dag_page(headers: HeaderMap) -> Html<String> {
    if is_htmx(&headers) {
        let frag = DagContentFragment;
        Html(frag.render().expect("dag content fragment"))
    } else {
        let page = DagPage;
        Html(page.render().expect("dag template"))
    }
}

#[derive(serde::Deserialize)]
pub struct DagRenderForm {
    pub source: String,
    pub format: Option<String>,
}

pub async fn dag_render(Form(form): Form<DagRenderForm>) -> Html<String> {
    let source = form.source.trim();
    if source.is_empty() {
        return Html(
            "<p style=\"color: var(--text-dim);\">Enter Polydat source to render</p>".into(),
        );
    }

    let fmt = form.format.as_deref().unwrap_or("svg");
    let result = match fmt {
        "svg" => viz::polydat_to_svg(source),
        "mermaid" => viz::polydat_to_mermaid(source).map(|m| format!("<pre>{}</pre>", esc(&m))),
        "dot" => viz::polydat_to_dot(source).map(|d| format!("<pre>{}</pre>", esc(&d))),
        _ => Err("unknown format".into()),
    };
    match result {
        Ok(content) => Html(content),
        Err(e) => Html(format!(
            "<pre style=\"color: var(--accent);\">Error: {}</pre>",
            esc(&e)
        )),
    }
}

// ─── Activities API ─────────────────────────────────────────

pub async fn activities_api() -> Html<String> {
    let fragment = ActivitiesFragment { activities: vec![] };
    Html(fragment.render().expect("activities_table fragment"))
}

// ─── Graph Editor ───────────────────────────────────────────

pub async fn graph_editor_page(headers: HeaderMap) -> Html<String> {
    if is_htmx(&headers) {
        let frag = GraphEditorContentFragment;
        Html(frag.render().expect("graph editor content fragment"))
    } else {
        let page = GraphEditorPage;
        Html(page.render().expect("graph editor template"))
    }
}

pub async fn graph_palette() -> axum::Json<Vec<crate::graph::PaletteCategory>> {
    axum::Json(crate::graph::build_palette())
}

pub async fn graph_compile(body: String) -> axum::Json<crate::graph::CompileResult> {
    axum::Json(crate::graph::compile_graph(&body))
}

pub async fn graph_eval(
    axum::Json(req): axum::Json<crate::graph::EvalRequest>,
) -> axum::Json<crate::graph::EvalResult> {
    axum::Json(crate::graph::eval_graph(req))
}

pub async fn graph_plot(
    axum::Json(req): axum::Json<crate::graph::PlotRequest>,
) -> axum::Json<crate::graph::PlotResult> {
    axum::Json(crate::graph::plot_graph(req))
}

// ─── Metrics Ingestion ──────────────────────────────────────

/// Accept metrics in Prometheus text exposition format.
///
/// Running `nmbrs run --web=host:port` sessions POST metrics here.
/// The parsed frame is published to all WebSocket subscribers.
pub async fn ingest_prometheus(
    State(broadcast): State<MetricsBroadcast>,
    body: String,
) -> StatusCode {
    let snapshot = openmetrics_parse::parse_prometheus_text(&body);
    if !snapshot.is_empty() {
        broadcast.publish(snapshot);
    }
    StatusCode::NO_CONTENT
}

// ─── Dynamic Controls (SRD 23) ──────────────────────────────

/// JSON view of a dynamic control for the API listing.
#[derive(serde::Serialize, Debug, Clone)]
pub struct ControlView {
    pub component: String,
    pub name: String,
    pub value_type: String,
    pub value: String,
    pub value_f64: Option<f64>,
    pub rev: u64,
    pub origin: String,
    pub scope: &'static str,
    pub final_scope: Option<String>,
    pub applier_count: usize,
    pub accepts_f64_writes: bool,
}

/// List every dynamic control declared in the running session's
/// component tree. Returns an empty list when no session is
/// active (e.g. standalone `nmbrs web` without a running run).
///
/// Endpoint: `GET /api/controls`
pub async fn list_controls() -> axum::Json<Vec<ControlView>> {
    use nmbrs_metrics::component::find;
    use nmbrs_metrics::selector::Selector;

    let mut views = Vec::new();
    let Some(root) = nmbrs_runtime::polydat_nodes::runtime_context::session_root_handle() else {
        return axum::Json(views);
    };
    for comp in find(&root, &Selector::new()) {
        let Ok(guard) = comp.read() else {
            continue;
        };
        let path = guard
            .effective_labels()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        for ctl in guard.controls().list() {
            views.push(ControlView {
                component: if path.is_empty() {
                    "<root>".into()
                } else {
                    path.clone()
                },
                name: ctl.name().to_string(),
                value_type: ctl.value_type_name().to_string(),
                value: ctl.value_string(),
                value_f64: ctl.gauge_f64(),
                rev: ctl.rev(),
                origin: format!("{:?}", ctl.origin()),
                scope: match ctl.branch_scope() {
                    nmbrs_metrics::controls::BranchScope::Local => "local",
                    nmbrs_metrics::controls::BranchScope::Subtree => "subtree",
                },
                final_scope: ctl.final_scope(),
                applier_count: ctl.applier_count(),
                accepts_f64_writes: ctl.accepts_f64_writes(),
            });
        }
    }
    views.sort_by(|a, b| {
        a.component
            .cmp(&b.component)
            .then_with(|| a.name.cmp(&b.name))
    });
    axum::Json(views)
}

/// Request body for a control write.
#[derive(serde::Deserialize, Debug)]
pub struct SetControlBody {
    pub value: f64,
    /// Caller identification for attribution in the control's
    /// committed-origin metadata. Any non-empty value is accepted;
    /// real deployments should pass a stable label (user id, bot
    /// name, automation handle). Defaults to `"api"` when absent.
    #[serde(default)]
    pub source: Option<String>,
}

/// Response body for a successful write dispatch.
#[derive(serde::Serialize, Debug)]
pub struct SetControlResponse {
    pub name: String,
    pub submitted_value: f64,
    pub committed_rev: u64,
}

/// Error body for write failures — parse / validation / final /
/// apply errors all funnel through this with a stable shape so
/// scripted callers can distinguish categories via `code`.
#[derive(serde::Serialize, Debug)]
pub struct SetControlError {
    pub name: String,
    pub code: &'static str,
    pub message: String,
}

/// Write a named dynamic control through the session root's
/// walk-up. Non-blocking: the write is awaited on the handler's
/// tokio task (not the event loop), so the applier runs to
/// completion before the response returns. Errors from the
/// control layer (validation, final-scope violation, apply
/// failure) surface as `400 Bad Request` with a structured
/// body; absent session root is `503 Service Unavailable`;
/// missing control name is `404 Not Found`.
///
/// Endpoint: `POST /api/control/{name}` with JSON body
/// `{"value": 42, "source": "operator"}`.
pub async fn set_control(
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::Json(body): axum::Json<SetControlBody>,
) -> Result<axum::Json<SetControlResponse>, (StatusCode, axum::Json<SetControlError>)> {
    let Some(root) = nmbrs_runtime::polydat_nodes::runtime_context::session_root_handle() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(SetControlError {
                name,
                code: "no_session",
                message: "no active session; cannot resolve controls".into(),
            }),
        ));
    };
    let erased = {
        let Ok(guard) = root.read() else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(SetControlError {
                    name,
                    code: "session_poisoned",
                    message: "session root RwLock is poisoned".into(),
                }),
            ));
        };
        match guard.find_control_erased_up(&name) {
            Some(e) => e,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    axum::Json(SetControlError {
                        name: name.clone(),
                        code: "not_found",
                        message: format!("no control named '{name}' via walk-up"),
                    }),
                ));
            }
        }
    };
    let source = body.source.clone().unwrap_or_else(|| "api".to_string());
    let origin = nmbrs_metrics::controls::ControlOrigin::Api { source };
    match erased.set_f64(body.value, origin).await {
        Ok(rev) => Ok(axum::Json(SetControlResponse {
            name,
            submitted_value: body.value,
            committed_rev: rev,
        })),
        Err(e) => {
            use nmbrs_metrics::controls::SetError;
            let (code, msg) = match &e {
                SetError::ValidationFailed(m) => ("validation_failed", m.clone()),
                SetError::ApplyFailed(fs) => (
                    "apply_failed",
                    fs.iter()
                        .map(|f| format!("#{}: {}", f.applier_index, f.message))
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                SetError::FinalViolation { scope } => (
                    "final_violation",
                    format!("control is final at scope '{scope}'"),
                ),
            };
            Err((
                StatusCode::BAD_REQUEST,
                axum::Json(SetControlError {
                    name,
                    code,
                    message: msg,
                }),
            ))
        }
    }
}

// ─── Scope Tree (SRD 18b) ───────────────────────────────────

/// Snapshot of the running session's scenario / scope tree.
///
/// Returns an empty `nodes` list (with `installed: false`) when no
/// session has published a scene tree yet — e.g. standalone
/// `nmbrs web` without a running run, or before pre-map completes.
/// During an active run the tree reflects the current pending /
/// running / completed / failed status of every concrete phase
/// and scope header; renderers can compute scope-level aggregate
/// status by walking children.
///
/// Endpoint: `GET /api/scope-tree`
pub async fn scope_tree() -> axum::Json<ScopeTreeResponse> {
    match nmbrs_runtime::scene_tree::current() {
        Some(tree) => axum::Json(ScopeTreeResponse {
            installed: true,
            tree: Some(tree),
        }),
        None => axum::Json(ScopeTreeResponse {
            installed: false,
            tree: None,
        }),
    }
}

#[derive(serde::Serialize)]
pub struct ScopeTreeResponse {
    pub installed: bool,
    pub tree: Option<nmbrs_runtime::scene_tree::SceneTree>,
}

// ─── Data Building ──────────────────────────────────────────

fn build_function_groups(filter: Option<&str>) -> Vec<(String, Vec<FunctionView>)> {
    let grouped = registry::by_category();
    let filter_lower = filter.map(|f| f.to_lowercase());
    let mut result = Vec::new();

    for (cat, funcs) in grouped {
        let views: Vec<FunctionView> = funcs
            .iter()
            .filter(|sig| match &filter_lower {
                Some(q) if !q.is_empty() => {
                    sig.name.contains(q.as_str())
                        || sig.description.to_lowercase().contains(q.as_str())
                }
                _ => true,
            })
            .map(|sig| {
                let const_info = sig.const_param_info();
                let params = if const_info.is_empty() {
                    String::new()
                } else {
                    let p: Vec<String> = const_info
                        .iter()
                        .map(|(name, req)| {
                            if *req {
                                name.to_string()
                            } else {
                                format!("[{name}]")
                            }
                        })
                        .collect();
                    format!("({})", p.join(", "))
                };
                let arity = if sig.outputs == 0 {
                    format!("{}\u{2192}N", sig.wire_input_count())
                } else {
                    format!("{}\u{2192}{}", sig.wire_input_count(), sig.outputs)
                };
                let level = nmbrs_runtime::bindings::probe_compile_level(sig.name);
                let (ls, lc) = match level {
                    registry::CompileLevel::Phase3 => ("P3", "green"),
                    registry::CompileLevel::Phase2 => ("P2", "yellow"),
                    registry::CompileLevel::Phase1 => ("P1", "blue"),
                };
                FunctionView {
                    name: sig.name.to_string(),
                    params_display: params,
                    arity_display: arity,
                    level: ls.to_string(),
                    level_class: lc.to_string(),
                    description: sig.description.to_string(),
                }
            })
            .collect();

        if !views.is_empty() {
            result.push((cat.display_name().to_string(), views));
        }
    }
    result
}

fn build_stdlib_groups() -> Vec<(String, Vec<StdlibModuleView>)> {
    use polydat::dsl::ast::Statement;
    use polydat::dsl::{lexer, parser};

    let sources = polydat::dsl::stdlib_sources();
    let mut result: Vec<(String, Vec<StdlibModuleView>)> = Vec::new();

    for (filename, source) in sources {
        let category = source
            .lines()
            .find(|l| l.trim().starts_with("// @category:"))
            .and_then(|l| l.trim().strip_prefix("// @category:"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| filename.replace(".polydat", ""));

        let tokens = match lexer::lex(source) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let ast = match parser::parse(tokens) {
            Ok(a) => a,
            Err(_) => continue,
        };

        let mut modules = Vec::new();
        for stmt in &ast.statements {
            if let Statement::ModuleDef(mdef) = stmt {
                let params: Vec<String> = mdef
                    .params
                    .iter()
                    .map(|p| format!("{}: {}", p.name, p.typ))
                    .collect();
                let outputs: Vec<String> = mdef
                    .outputs
                    .iter()
                    .map(|o| format!("{}: {}", o.name, o.typ))
                    .collect();
                let sig = format!("({}) \u{2192} ({})", params.join(", "), outputs.join(", "));

                modules.push(StdlibModuleView {
                    name: mdef.name.clone(),
                    signature: sig,
                    description: String::new(),
                });
            }
        }
        if !modules.is_empty() {
            result.push((category, modules));
        }
    }
    result
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
