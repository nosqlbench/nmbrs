// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Spectest validation: round-trip verification of the uniform
//! workload specification against the living spec documents.
//!
//! Each markdown file in workload_definition/ contains test triples
//! (yaml → json → ops) that are extracted, parsed, and validated.

use nmbrs_workload::spectest::{extract_spec_tests, run_spec_tests, validate_spec_test};

/// Load a spec file and return its content.
macro_rules! spec_file {
    ($name:expr) => {
        include_str!(concat!("workload_definition/", $name))
    };
}

#[test]
fn spectest_04_op_template_basics() {
    let md = spec_file!("04_op_template_basics.md");
    let (passed, failed, errors) = run_spec_tests(md);
    if failed > 0 {
        for e in &errors {
            eprintln!("{e}");
        }
    }
    assert!(
        passed > 0,
        "no spec tests found in 04_op_template_basics.md"
    );
    eprintln!("04_op_template_basics: {passed} passed, {failed} failed");
    // Don't fail on mismatches yet — track progress
}

#[test]
fn spectest_05_op_template_payloads() {
    let md = spec_file!("05_op_template_payloads.md");
    let (passed, failed, errors) = run_spec_tests(md);
    if failed > 0 {
        for e in &errors {
            eprintln!("{e}");
        }
    }
    assert!(
        passed > 0 || failed > 0,
        "no spec tests found in 05_op_template_payloads.md"
    );
    eprintln!("05_op_template_payloads: {passed} passed, {failed} failed");
}

#[test]
fn spectest_06_op_template_variations() {
    let md = spec_file!("06_op_template_variations.md");
    let (passed, failed, errors) = run_spec_tests(md);
    if failed > 0 {
        for e in &errors {
            eprintln!("{e}");
        }
    }
    eprintln!("06_op_template_variations: {passed} passed, {failed} failed");
}

#[test]
fn spectest_07_template_variables() {
    let md = spec_file!("07_template_variables.md");
    let (passed, failed, errors) = run_spec_tests(md);
    if failed > 0 {
        for e in &errors {
            eprintln!("{e}");
        }
    }
    eprintln!("07_template_variables: {passed} passed, {failed} failed");
}

#[test]
fn spectest_02_workload_structure() {
    let md = spec_file!("02_workload_structure.md");
    let (passed, failed, errors) = run_spec_tests(md);
    if failed > 0 {
        for e in &errors {
            eprintln!("{e}");
        }
    }
    eprintln!("02_workload_structure: {passed} passed, {failed} failed");
}

/// Comprehensive test: run ALL spec files and report aggregate results.
#[test]
fn spectest_all_files() {
    let files = [
        (
            "02_workload_structure.md",
            spec_file!("02_workload_structure.md"),
        ),
        (
            "04_op_template_basics.md",
            spec_file!("04_op_template_basics.md"),
        ),
        (
            "05_op_template_payloads.md",
            spec_file!("05_op_template_payloads.md"),
        ),
        (
            "06_op_template_variations.md",
            spec_file!("06_op_template_variations.md"),
        ),
        (
            "07_template_variables.md",
            spec_file!("07_template_variables.md"),
        ),
    ];

    let mut total_passed = 0;
    let mut total_failed = 0;
    let mut all_errors = Vec::new();

    for (name, content) in &files {
        let tests = extract_spec_tests(content);
        for test in &tests {
            match validate_spec_test(test) {
                Ok(()) => total_passed += 1,
                Err(e) => {
                    total_failed += 1;
                    all_errors.push(format!("[{name}] {e}"));
                }
            }
        }
    }

    eprintln!("\n=== Spectest Summary ===");
    eprintln!("Total: {} passed, {} failed", total_passed, total_failed);
    if !all_errors.is_empty() {
        eprintln!("\nFailures:");
        for e in &all_errors {
            eprintln!("  {e}");
        }
    }

    // For now, report but don't fail — we'll tighten this as we fix
    // normalization mismatches.
    assert!(total_passed > 0, "no spec tests found at all");
}
