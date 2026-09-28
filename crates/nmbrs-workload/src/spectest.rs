// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SpecTest: parse and validate workload specification examples.
//!
//! The workload_definition markdown files contain test triples:
//! 1. `*yaml:*` — YAML source
//! 2. `*json:*` — JSON equivalent (validated against YAML parse)
//! 3. `*ops:*`  — ParsedOp API view (validated against normalization)
//!
//! This module parses those triples from markdown and validates them.

/// A single spectest triple extracted from a markdown file.
#[derive(Debug, Clone)]
pub struct SpecTestCase {
    pub title: String,
    pub yaml_source: String,
    pub json_expected: String,
    pub ops_expected: String,
    pub line_number: usize,
}

/// Extract all spectest triples from a markdown file.
pub fn extract_spec_tests(markdown: &str) -> Vec<SpecTestCase> {
    let mut tests = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0;
    let mut current_title = String::new();

    while i < lines.len() {
        let line = lines[i].trim();

        // Track headings for test titles
        if line.starts_with('#') {
            current_title = line.trim_start_matches('#').trim().to_string();
        }

        // Look for *yaml:* marker
        if line == "*yaml:*" {
            let yaml_line = i + 1;
            let yaml = extract_code_block(&lines, &mut i);
            let json = if i < lines.len() && lines[i].trim() == "*json:*" {
                extract_code_block(&lines, &mut i)
            } else {
                String::new()
            };
            let ops = if i < lines.len() && lines[i].trim() == "*ops:*" {
                extract_code_block(&lines, &mut i)
            } else {
                String::new()
            };

            if !yaml.is_empty() {
                tests.push(SpecTestCase {
                    title: current_title.clone(),
                    yaml_source: yaml,
                    json_expected: json,
                    ops_expected: ops,
                    line_number: yaml_line,
                });
            }
        } else {
            i += 1;
        }
    }

    tests
}

/// Extract the content of a fenced code block starting after the current line.
fn extract_code_block(lines: &[&str], pos: &mut usize) -> String {
    *pos += 1; // skip the marker line

    // Find opening fence
    while *pos < lines.len() {
        let trimmed = lines[*pos].trim();
        if trimmed.starts_with("```") {
            *pos += 1;
            break;
        }
        *pos += 1;
    }

    // Collect content until closing fence
    let mut content = Vec::new();
    while *pos < lines.len() {
        let trimmed = lines[*pos].trim();
        if trimmed.starts_with("```") {
            *pos += 1;
            break;
        }
        content.push(lines[*pos]);
        *pos += 1;
    }

    content.join("\n").trim().to_string()
}

/// Validate a single spectest case.
///
/// Returns `Ok(())` if the YAML parses correctly and the normalized
/// ops match the expected ops JSON. Returns `Err(message)` on failure.
pub fn validate_spec_test(test: &SpecTestCase) -> Result<(), String> {
    // Stage 1: Parse YAML
    let yaml_value: serde_json::Value = serde_yaml::from_str(&test.yaml_source)
        .map_err(|e| format!("[line {}] YAML parse error: {e}", test.line_number))?;

    // Stage 2: Validate YAML↔JSON equivalence (if json provided)
    if !test.json_expected.is_empty() {
        let json_value: serde_json::Value =
            serde_json::from_str(&test.json_expected).map_err(|e| {
                format!(
                    "[line {}] JSON parse error in expected: {e}",
                    test.line_number
                )
            })?;

        if !json_values_equivalent(&yaml_value, &json_value) {
            return Err(format!(
                "[line {}] YAML↔JSON mismatch in '{}'\n  YAML parsed as: {}\n  JSON expected:  {}",
                test.line_number,
                test.title,
                serde_json::to_string(&yaml_value).unwrap_or_default(),
                serde_json::to_string(&json_value).unwrap_or_default(),
            ));
        }
    }

    // Stage 3: Validate normalized ops (if ops provided)
    if !test.ops_expected.is_empty() && test.ops_expected != "[]" {
        let expected_ops: serde_json::Value = serde_json::from_str(&test.ops_expected)
            .map_err(|e| format!("[line {}] Ops JSON parse error: {e}", test.line_number))?;

        // Parse through our workload parser
        let parsed = crate::parse::parse_ops(&test.yaml_source)
            .map_err(|e| format!("[line {}] Workload parse error: {e}", test.line_number))?;

        // Convert our ParsedOps to JSON for comparison
        let our_ops: serde_json::Value = serde_json::to_value(&parsed)
            .map_err(|e| format!("[line {}] Serialization error: {e}", test.line_number))?;

        // Compare each expected op against our output
        if let serde_json::Value::Array(expected_arr) = &expected_ops
            && let serde_json::Value::Array(our_arr) = &our_ops
        {
            for (idx, expected_op) in expected_arr.iter().enumerate() {
                // Find matching op by name
                let expected_name = expected_op
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let our_op = our_arr
                    .iter()
                    .find(|o| o.get("name").and_then(|v| v.as_str()) == Some(expected_name));

                if let Some(our_op) = our_op {
                    // Compare key fields
                    validate_op_fields(expected_op, our_op, &test.title, test.line_number, idx)?;
                } else {
                    return Err(format!(
                        "[line {}] Op '{}' expected but not found in output for '{}'",
                        test.line_number, expected_name, test.title,
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Compare two JSON values for structural equivalence.
/// Order-insensitive for objects, order-sensitive for arrays.
fn json_values_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Object(am), serde_json::Value::Object(bm)) => {
            // All keys in a must be in b with equivalent values
            am.iter()
                .all(|(k, v)| bm.get(k).is_some_and(|bv| json_values_equivalent(v, bv)))
                && bm.iter().all(|(k, _)| am.contains_key(k))
        }
        (serde_json::Value::Array(aa), serde_json::Value::Array(ba)) => {
            aa.len() == ba.len()
                && aa
                    .iter()
                    .zip(ba.iter())
                    .all(|(a, b)| json_values_equivalent(a, b))
        }
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a == b,
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a == b,
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a == b,
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        _ => false,
    }
}

/// Validate key fields of a parsed op against expected.
fn validate_op_fields(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    title: &str,
    line: usize,
    idx: usize,
) -> Result<(), String> {
    // Check name
    let exp_name = expected.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let act_name = actual.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if exp_name != act_name {
        return Err(format!(
            "[line {line}] Op {idx} name mismatch in '{title}': expected '{exp_name}', got '{act_name}'"
        ));
    }

    // Check op fields
    if let Some(exp_op) = expected.get("op")
        && let Some(act_op) = actual.get("op")
        && !json_values_equivalent(exp_op, act_op)
    {
        return Err(format!(
            "[line {line}] Op '{exp_name}' op fields mismatch in '{title}'\n  expected: {}\n  actual:   {}",
            serde_json::to_string(exp_op).unwrap_or_default(),
            serde_json::to_string(act_op).unwrap_or_default(),
        ));
    }

    // Check tags (if expected has them)
    if let Some(exp_tags) = expected.get("tags")
        && let Some(act_tags) = actual.get("tags")
        && let (Some(exp_map), Some(act_map)) = (exp_tags.as_object(), act_tags.as_object())
    {
        for (key, exp_val) in exp_map {
            if let Some(act_val) = act_map.get(key) {
                if exp_val != act_val {
                    return Err(format!(
                        "[line {line}] Op '{exp_name}' tag '{key}' mismatch in '{title}': expected {exp_val}, got {act_val}"
                    ));
                }
            } else {
                return Err(format!(
                    "[line {line}] Op '{exp_name}' missing tag '{key}' in '{title}'"
                ));
            }
        }
    }

    // Check bindings (if expected has them)
    if let Some(exp_bindings) = expected.get("bindings")
        && let Some(act_bindings) = actual.get("bindings")
        && !json_values_equivalent(exp_bindings, act_bindings)
    {
        return Err(format!(
            "[line {line}] Op '{exp_name}' bindings mismatch in '{title}'"
        ));
    }

    // Check params (if expected has them)
    if let Some(exp_params) = expected.get("params")
        && let Some(act_params) = actual.get("params")
        && !json_values_equivalent(exp_params, act_params)
    {
        return Err(format!(
            "[line {line}] Op '{exp_name}' params mismatch in '{title}'"
        ));
    }

    Ok(())
}

/// Run all spectest cases from a markdown file.
/// Returns (passed, failed, errors).
pub fn run_spec_tests(markdown: &str) -> (usize, usize, Vec<String>) {
    let tests = extract_spec_tests(markdown);
    let mut passed = 0;
    let mut failed = 0;
    let mut errors = Vec::new();

    for test in &tests {
        match validate_spec_test(test) {
            Ok(()) => passed += 1,
            Err(e) => {
                failed += 1;
                errors.push(e);
            }
        }
    }

    (passed, failed, errors)
}
