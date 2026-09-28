// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The one predicate mechanism.
//!
//! Several wrappers gate on a workload-authored polydat expression: `if:`
//! skips a cycle, `while:` keeps looping, `poll:` decides it is done. They are
//! the same operation — evaluate a named binding against the executing node's
//! kernel and ask whether it is truthy — and they were drifting into separate
//! implementations, with `is_truthy` copied between `if.rs` and `while.rs`
//! under a comment noting the two "share a semantic". A shared semantic
//! maintained by comment is a semantic that will diverge.
//!
//! **Node-type agnostic by construction.** Nothing here knows whether the
//! predicate belongs to an op template, a phase, or something added later. It
//! reads through [`WireSource`], which is whichever polydat kernel instance is
//! in scope for the node being executed. A predicate synthesised into an
//! op-template kernel and one synthesised into a phase kernel are read by the
//! same call; scoping decides which binding is found, exactly as it does for
//! every other wire.

use polydat::ast::Value;

use crate::wires::WireSource;

/// The wire a poll's `until:` expression is lowered to.
///
/// One name across node types on purpose: a phase's `poll.until` and an op's
/// `poll.until` compile to the same binding in their respective kernels, so
/// the reader does not care which it got. Ordinary scoping resolves it — an
/// op-template binding shadows the phase's, as any other wire would.
pub const UNTIL_BINDING: &str = "__poll_until";

/// Truthiness for every workload-authored predicate in the runtime.
///
/// `None`, zero, `false`, and the empty string are falsy; everything else is
/// truthy. Deliberately permissive about type: a workload may write
/// `count > 0`, a bare captured `ok`, or a string, and all three mean the
/// obvious thing.
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::None => false,
        Value::U64(v) => *v != 0,
        Value::F64(v) => *v != 0.0,
        Value::Bool(v) => *v,
        Value::Str(s) => !s.is_empty(),
        _ => true,
    }
}

/// Evaluate the named predicate against the executing node's kernel.
///
/// `None` when the binding does not resolve — which is a wiring fault, not a
/// false predicate, and callers must not conflate the two. Treating an
/// unresolved condition as "false" would make a poll spin until its timeout
/// and a `while` exit immediately, both silently.
pub fn holds(wires: &dyn WireSource, binding: &str) -> Option<bool> {
    wires.get(binding).map(|v| is_truthy(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falsy_values_are_exactly_none_zero_false_and_empty() {
        assert!(!is_truthy(&Value::None));
        assert!(!is_truthy(&Value::U64(0)));
        assert!(!is_truthy(&Value::F64(0.0)));
        assert!(!is_truthy(&Value::Bool(false)));
        assert!(!is_truthy(&Value::Str(String::new().into())));
    }

    #[test]
    fn everything_else_is_truthy() {
        assert!(is_truthy(&Value::U64(1)));
        assert!(is_truthy(&Value::F64(-0.5)));
        assert!(is_truthy(&Value::Bool(true)));
        assert!(is_truthy(&Value::Str("no".to_string().into())));
    }

    /// An unresolved binding is not a false predicate. A caller that folded
    /// the two together would make a poll spin to its timeout and a `while`
    /// exit at once, neither of them saying why.
    #[test]
    fn an_unresolved_binding_is_distinguishable_from_false() {
        struct OnlyX(bool);
        impl WireSource for OnlyX {
            fn get(&self, name: &str) -> Option<Value> {
                (name == "x").then(|| Value::Bool(self.0))
            }
            fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
                Box::new(std::iter::once("x".to_string()))
            }
        }
        assert_eq!(holds(&OnlyX(false), "x"), Some(false));
        assert_eq!(holds(&OnlyX(true), "x"), Some(true));
        assert_eq!(holds(&OnlyX(true), "missing"), None);
    }
}
