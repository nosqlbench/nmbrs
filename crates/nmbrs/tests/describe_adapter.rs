// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs describe adapter <name>` routing — both the `topic=value` shorthand
//! (`describe adapter=stdout`) and the positional form (`describe adapter
//! stdout`) must reach the adapter-detail renderer. The `=` form used to be
//! dropped by the CLI-spec walker (it isn't an exact subcommand match), so it
//! fell through to the topic-list help; the default handler now forwards the
//! leftover token to `describe_command`, which resolves the `=`-shorthand.

use std::process::Command;

fn describe(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("describe")
        .args(args)
        .output()
        .expect("run `nmbrs describe …`");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn adapter_equals_shorthand_reaches_adapter_detail() {
    let out = describe(&["adapter=stdout"]);
    assert!(
        out.contains("Adapter: stdout"),
        "`describe adapter=stdout` must render the adapter, not the topic list:\n{out}"
    );
}

#[test]
fn adapter_positional_form_reaches_adapter_detail() {
    let out = describe(&["adapter", "stdout"]);
    assert!(
        out.contains("Adapter: stdout"),
        "`describe adapter stdout` must render the adapter:\n{out}"
    );
}

#[test]
fn bare_adapter_lists_registered_adapters() {
    let out = describe(&["adapter"]);
    assert!(
        out.contains("Registered adapters:"),
        "`describe adapter` must list adapters:\n{out}"
    );
}
