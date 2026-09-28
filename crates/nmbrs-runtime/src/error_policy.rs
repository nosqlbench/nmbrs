// Copyright (c) nosqlbench
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
// implied. See the License for the specific language governing
// permissions and limitations under the License.

//! SRD-82 — [`ErrorPolicy`]: a shell's error handler, which is also
//! its own resolver for child shells.
//!
//! Resolving a shell's error policy is an **init-time** action with
//! two moments:
//!
//!   - **Depth (descend):** a child shell inherits its parent's policy
//!     unless it overrides. [`ErrorPolicy::resolve_child`] with `None`
//!     (or a config equal to the parent's) returns the parent policy
//!     itself — no new instance, and no re-resolution within the shell
//!     once bound.
//!   - **Breadth (within a layer):** sibling shells that override with
//!     the *same* config share one derived policy, deduplicated by the
//!     config's value-equality.
//!
//! Both live in one type, so there is no separate dispenser service to
//! thread alongside the handler: an `ErrorPolicy` *is* the resolver for
//! its children. The session holds a **root** policy (the default);
//! each shell binds its own at scope-init by resolving from its
//! parent's, and holds the reference thereafter.

use nmbrs_errorhandler::ErrorRouter;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The content that determines an [`ErrorPolicy`]. Equal configs
/// resolve to one shared instance (the breadth value-equality key).
/// `error_rate_max` is carried as raw bits so the config is
/// `Hash`/`Eq` (f64 is neither).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PolicyConfig {
    error_spec: String,
    error_rate_max_bits: Option<u64>,
}

impl PolicyConfig {
    pub fn new(error_spec: impl Into<String>, error_rate_max: Option<f64>) -> Self {
        PolicyConfig {
            error_spec: error_spec.into(),
            error_rate_max_bits: error_rate_max.map(f64::to_bits),
        }
    }
    pub fn error_spec(&self) -> &str {
        &self.error_spec
    }
    pub fn error_rate_max(&self) -> Option<f64> {
        self.error_rate_max_bits.map(f64::from_bits)
    }
}

/// A shell's composed error policy — the op-error router plus the
/// aggregate guard — which is ALSO the resolver for its child shells.
/// SRD-82.
///
/// The session creates a root via [`ErrorPolicy::root`]; each shell
/// binds its own at scope-init via [`ErrorPolicy::resolve_child`],
/// inheriting the parent (depth) or deriving a value-equality-shared
/// instance (breadth). Once bound, a shell never re-resolves.
pub struct ErrorPolicy {
    config: PolicyConfig,
    /// Per-op error routing (error-name → count/warn/retry/stop).
    pub router: ErrorRouter,
    /// Breadth cache: a child's config → its derived shared policy.
    /// Empty until a child overrides the inherited config.
    derived: Mutex<HashMap<PolicyConfig, Arc<ErrorPolicy>>>,
}

impl ErrorPolicy {
    /// The session root policy — the default every shell inherits
    /// until one overrides.
    pub fn root(config: PolicyConfig) -> Arc<Self> {
        Arc::new(Self::build(config))
    }

    /// A standalone policy with no parent — the library/test path
    /// where no session root exists.
    pub fn standalone(config: PolicyConfig) -> Arc<Self> {
        Self::root(config)
    }

    fn build(config: PolicyConfig) -> Self {
        let router =
            ErrorRouter::parse(config.error_spec()).unwrap_or_else(|_| ErrorRouter::default_stop());
        ErrorPolicy {
            config,
            router,
            derived: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the policy for a child shell at scope-init. `None` (no
    /// override) or a config equal to this policy's → inherit `self`
    /// (depth, shared by reference). A differing config → derive a
    /// child policy, deduplicated across siblings by value-equality
    /// (breadth).
    pub fn resolve_child(self: &Arc<Self>, child: Option<PolicyConfig>) -> Arc<ErrorPolicy> {
        match child {
            None => self.clone(),
            Some(cfg) if cfg == self.config => self.clone(),
            Some(cfg) => {
                let mut derived = self.derived.lock().unwrap_or_else(|e| e.into_inner());
                derived
                    .entry(cfg.clone())
                    .or_insert_with(|| Arc::new(ErrorPolicy::build(cfg)))
                    .clone()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(spec: &str, rate: Option<f64>) -> PolicyConfig {
        PolicyConfig::new(spec, rate)
    }

    #[test]
    fn inherits_parent_on_no_override() {
        let root = ErrorPolicy::root(cfg(".*:warn,stop", Some(0.1)));
        let child = root.resolve_child(None);
        assert!(
            Arc::ptr_eq(&root, &child),
            "no override inherits the parent (depth)"
        );
    }

    #[test]
    fn inherits_parent_on_equal_config() {
        let root = ErrorPolicy::root(cfg(".*:warn,stop", Some(0.1)));
        let child = root.resolve_child(Some(cfg(".*:warn,stop", Some(0.1))));
        assert!(
            Arc::ptr_eq(&root, &child),
            "equal config inherits, no new instance"
        );
    }

    #[test]
    fn siblings_with_equal_override_share_one_instance() {
        let root = ErrorPolicy::root(cfg(".*:warn,stop", Some(0.1)));
        // Two siblings both override with the same config.
        let a = root.resolve_child(Some(cfg("Timeout:retry;.*:stop", Some(0.2))));
        let b = root.resolve_child(Some(cfg("Timeout:retry;.*:stop", Some(0.2))));
        assert!(
            Arc::ptr_eq(&a, &b),
            "equal overrides share one derived instance (breadth)"
        );
        assert!(
            !Arc::ptr_eq(&root, &a),
            "an override is a new instance, not the parent"
        );
    }

    #[test]
    fn distinct_overrides_get_distinct_instances() {
        let root = ErrorPolicy::root(cfg(".*:warn,stop", Some(0.1)));
        let a = root.resolve_child(Some(cfg(".*:stop", Some(0.2))));
        let b = root.resolve_child(Some(cfg(".*:warn", Some(0.3))));
        assert!(!Arc::ptr_eq(&a, &b));
    }
}
