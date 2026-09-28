// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Workload generator: converts API operations into ParsedOps.
//!
//! Each ApiOperation becomes a ParsedOp with fields for method, URI,
//! body (if POST/PUT/PATCH), and content_type. Path parameters and
//! body fields become Polydat bind points. Query parameters become part
//! of the URI template.

use crate::spec::{ApiOperation, FieldInfo};
use nmbrs_workload::model::ParsedOp;
use std::collections::HashMap;

/// Generate ParsedOps from API operations.
///
/// Each operation becomes one ParsedOp. The op fields are:
/// - `method`: HTTP method
/// - `uri`: URL path with {param} bind points
/// - `body`: JSON body template with {field} bind points (for POST/PUT/PATCH)
/// - `content_type`: from the request body spec
///
/// Polydat bindings are auto-generated for path parameters and body fields.
pub fn generate_ops(ops: &[ApiOperation], base_url: &str) -> (Vec<ParsedOp>, String) {
    let mut parsed_ops = Vec::new();
    let mut binding_lines = Vec::new();
    let mut seen_bindings: HashMap<String, bool> = HashMap::new();

    let base = base_url.trim_end_matches('/');

    for api_op in ops {
        let mut op_fields: HashMap<String, serde_json::Value> = HashMap::new();

        // Method
        op_fields.insert(
            "method".into(),
            serde_json::Value::String(api_op.method.clone()),
        );

        // URI: base_url + path with bind points
        let mut uri = format!("{base}{}", api_op.path);

        // Add query parameters as bind points in the query string
        if !api_op.query_params.is_empty() {
            let qp: Vec<String> = api_op
                .query_params
                .iter()
                .map(|p| format!("{}={{{}}}", p.name, p.name))
                .collect();
            uri = format!("{uri}?{}", qp.join("&"));
        }
        op_fields.insert("uri".into(), serde_json::Value::String(uri));

        // Generate bindings for path parameters
        for param in &api_op.path_params {
            if !seen_bindings.contains_key(&param.name) {
                binding_lines.push(polydat_binding_for_param(&param.name, &param.schema_type));
                seen_bindings.insert(param.name.clone(), true);
            }
        }

        // Generate bindings for query parameters
        for param in &api_op.query_params {
            if !seen_bindings.contains_key(&param.name) {
                binding_lines.push(polydat_binding_for_param(&param.name, &param.schema_type));
                seen_bindings.insert(param.name.clone(), true);
            }
        }

        // Body for POST/PUT/PATCH
        if let Some(body_info) = &api_op.request_body {
            op_fields.insert(
                "content_type".into(),
                serde_json::Value::String(body_info.content_type.clone()),
            );

            let body_template = generate_body_template(&body_info.fields);
            op_fields.insert("body".into(), serde_json::Value::String(body_template));

            // Generate bindings for body fields
            for field in &body_info.fields {
                let bind_name = field.name.replace('.', "_");
                if let std::collections::hash_map::Entry::Vacant(e) =
                    seen_bindings.entry(bind_name.clone())
                {
                    binding_lines.push(polydat_binding_for_param(&bind_name, &field.schema_type));
                    e.insert(true);
                }
            }
        }

        let parsed = ParsedOp {
            traverse: None,
            name: api_op.operation_id.clone(),
            description: if api_op.summary.is_empty() {
                None
            } else {
                Some(api_op.summary.clone())
            },
            op: op_fields,
            bindings: Default::default(),
            params: HashMap::new(),
            tags: api_op
                .tags
                .iter()
                .map(|t| (t.clone(), "true".into()))
                .collect(),
            condition: None,
            delay: None,
            metrics: HashMap::new(),
            result: None,
            wrappers: None,
            captures: Vec::new(),
            abstract_interface: None,
            interface_bound: false,
            daemon: nmbrs_workload::model::DaemonSpec::Disabled,
            daemon_cancel_grace_ms: None,
            while_cond: None,
            rate: None,
        };
        parsed_ops.push(parsed);
    }

    let bindings_source = if binding_lines.is_empty() {
        String::new()
    } else {
        binding_lines.join("\n")
    };

    (parsed_ops, bindings_source)
}

/// Generate a Polydat binding expression for a parameter based on its schema type.
fn polydat_binding_for_param(name: &str, schema_type: &str) -> String {
    match schema_type {
        "integer" => format!("{name} := mod(hash(cycle), 1000000)"),
        "number" => format!("{name} := unit_interval(hash(cycle))"),
        "boolean" => format!("{name} := fair_coin(hash(cycle))"),
        "string" => format!("{name} := combinations(hash(cycle), \"a-z0-9\", 8)"),
        _ => format!("{name} := combinations(hash(cycle), \"a-z0-9\", 8)"),
    }
}

/// Generate a JSON body template with bind points for each field.
fn generate_body_template(fields: &[FieldInfo]) -> String {
    let pairs: Vec<String> = fields
        .iter()
        .map(|f| {
            let bind_name = f.name.replace('.', "_");
            match f.schema_type.as_str() {
                "integer" | "number" | "boolean" => {
                    format!("\"{}\": {{{bind_name}}}", f.name)
                }
                _ => {
                    format!("\"{}\": \"{{{bind_name}}}\"", f.name)
                }
            }
        })
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::*;

    #[test]
    fn generate_simple_get() {
        let ops = vec![ApiOperation {
            method: "GET".into(),
            path: "/users/{userId}".into(),
            operation_id: "getUser".into(),
            summary: "Get a user".into(),
            path_params: vec![ParamInfo {
                name: "userId".into(),
                schema_type: "integer".into(),
                required: true,
            }],
            query_params: vec![],
            request_body: None,
            tags: vec![],
        }];

        let (parsed, bindings) = generate_ops(&ops, "http://localhost:8080");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "getUser");
        let uri = parsed[0].op["uri"].as_str().unwrap();
        assert!(uri.contains("/users/{userId}"));
        assert!(uri.starts_with("http://localhost:8080"));
        assert!(bindings.contains("userId"));
    }

    #[test]
    fn generate_post_with_body() {
        let ops = vec![ApiOperation {
            method: "POST".into(),
            path: "/users".into(),
            operation_id: "createUser".into(),
            summary: String::new(),
            path_params: vec![],
            query_params: vec![],
            request_body: Some(BodyInfo {
                content_type: "application/json".into(),
                fields: vec![
                    FieldInfo {
                        name: "name".into(),
                        schema_type: "string".into(),
                        required: true,
                    },
                    FieldInfo {
                        name: "age".into(),
                        schema_type: "integer".into(),
                        required: false,
                    },
                ],
            }),
            tags: vec![],
        }];

        let (parsed, bindings) = generate_ops(&ops, "http://localhost:8080");
        let body = parsed[0].op["body"].as_str().unwrap();
        assert!(body.contains("\"name\": \"{name}\""));
        assert!(body.contains("\"age\": {age}"));
        assert!(bindings.contains("name :="));
        assert!(bindings.contains("age :="));
    }
}
