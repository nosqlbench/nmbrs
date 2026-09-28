// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-23 — end-to-end test of the dynamic-control **capability** catalog
//! through the real binary. `nmbrs describe controls` renders every control the
//! binary *can* declare (core + each registered adapter's `supported_controls`),
//! with its `declared_when` condition — the static discovery surface that fixes
//! the `rate`-isn't-as-visible-as-`concurrency` asymmetry: a conditional
//! control is now discoverable without instantiating the component tree.

use std::process::Command;

#[test]
fn describe_controls_lists_core_controls_with_conditions() {
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .args(["describe", "controls"])
        .output()
        .expect("run `nmbrs describe controls`");
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Both core controls are listed regardless of whether a workload declares
    // them — that is the whole point of the capability tier.
    assert!(
        stdout.contains("concurrency"),
        "concurrency missing:\n{stdout}"
    );
    assert!(stdout.contains("rate"), "rate missing:\n{stdout}");

    // The *condition* is surfaced — concurrency is always there, rate only when
    // a phase sets `rate:`. This is the discoverability the user asked for.
    assert!(
        stdout.contains("always"),
        "concurrency's `always` condition missing:\n{stdout}"
    );
    assert!(
        stdout.contains("when a phase sets `rate:`"),
        "rate's declaration condition missing:\n{stdout}"
    );
}

#[test]
fn describe_control_detail_shows_servo_form() {
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .args(["describe", "controls", "rate"])
        .output()
        .expect("run `nmbrs describe controls rate`");
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Control: rate"),
        "no detail header:\n{stdout}"
    );
    // The detail view points at the SRD-86 servoing surface for the control.
    assert!(
        stdout.contains("servo: rate"),
        "detail must show the direct servo form:\n{stdout}"
    );
}

#[test]
fn describe_unknown_control_is_rejected_with_the_listing() {
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .args(["describe", "controls", "no_such_control"])
        .output()
        .expect("run `nmbrs describe controls no_such_control`");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("No dynamic control named 'no_such_control'"),
        "unknown control must be rejected:\n{combined}"
    );
    // ...and it falls back to the full listing so the user sees the real names.
    assert!(
        combined.contains("concurrency"),
        "no fallback listing:\n{combined}"
    );
}
