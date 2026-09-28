// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Askama template structs and view models for the web UI.

use askama::Template;

// ─── View Models ────────────────────────────────────────────

/// A function entry for the function browser.
#[derive(Debug, Clone)]
pub struct FunctionView {
    pub name: String,
    pub params_display: String,
    pub arity_display: String,
    pub level: String,
    pub level_class: String,
    pub description: String,
}

/// A stdlib module entry for the module browser.
#[derive(Debug, Clone)]
pub struct StdlibModuleView {
    pub name: String,
    pub signature: String,
    pub description: String,
}

/// An activity status entry for the dashboard.
#[derive(Debug, Clone)]
pub struct ActivityView {
    pub name: String,
    pub status: String,
    pub status_class: String,
    pub cycles_done: u64,
    pub cycles_total: u64,
    pub ops_per_sec: String,
    pub error_count: u64,
}

// ─── Full-Page Templates ────────────────────────────────────

/// Dashboard page — extends `base.html`.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardPage {
    pub total_cycles: String,
    pub ops_per_sec: String,
    pub p99_ms: String,
    pub error_count: String,
    pub activities: Vec<ActivityView>,
}

/// Function browser page — extends `base.html`.
#[derive(Template)]
#[template(path = "functions.html")]
pub struct FunctionsPage {
    pub groups: Vec<(String, Vec<FunctionView>)>,
}

/// Stdlib browser page — extends `base.html`.
#[derive(Template)]
#[template(path = "stdlib.html")]
pub struct StdlibPage {
    pub groups: Vec<(String, Vec<StdlibModuleView>)>,
}

/// DAG viewer page — extends `base.html`.
#[derive(Template)]
#[template(path = "dag.html")]
pub struct DagPage;

// ─── Fragment Templates ─────────────────────────────────────

/// Activities table fragment (htmx partial).
#[derive(Template)]
#[template(path = "fragments/activities_table.html")]
pub struct ActivitiesFragment {
    pub activities: Vec<ActivityView>,
}

/// Function table fragment (htmx partial).
#[derive(Template)]
#[template(path = "fragments/function_table.html")]
pub struct FunctionTableFragment {
    pub groups: Vec<(String, Vec<FunctionView>)>,
}

/// Dashboard content fragment (htmx navigation).
#[derive(Template)]
#[template(path = "fragments/dashboard_content.html")]
pub struct DashboardContentFragment {
    pub total_cycles: String,
    pub ops_per_sec: String,
    pub p99_ms: String,
    pub error_count: String,
    pub activities: Vec<ActivityView>,
}

/// Functions content fragment (htmx navigation).
#[derive(Template)]
#[template(path = "fragments/functions_content.html")]
pub struct FunctionsContentFragment {
    pub groups: Vec<(String, Vec<FunctionView>)>,
}

/// Stdlib content fragment (htmx navigation).
#[derive(Template)]
#[template(path = "fragments/stdlib_content.html")]
pub struct StdlibContentFragment {
    pub groups: Vec<(String, Vec<StdlibModuleView>)>,
}

/// DAG viewer content fragment (htmx navigation).
#[derive(Template)]
#[template(path = "fragments/dag_content.html")]
pub struct DagContentFragment;

/// Graph editor page — extends `base.html`.
#[derive(Template)]
#[template(path = "graph_editor.html")]
pub struct GraphEditorPage;

/// Graph editor content fragment (htmx navigation).
#[derive(Template)]
#[template(path = "fragments/graph_editor_content.html")]
pub struct GraphEditorContentFragment;
