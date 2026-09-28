// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Memo wrapper — publishes a short human-visible string to the
//! activity's `memo` ArcSwap before / after the inner op runs.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("memo");

/// Trigger: `memo:` is either a bare string (shorthand) or a
/// map with `before:` / `after:` keys.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template
        .params
        .get("memo")
        .map(|v| v.is_string() || v.is_object())
        .unwrap_or(false)
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    let v = template.params.get("memo")?;
    if let Some(s) = v.as_str() {
        if s.is_empty() {
            return None;
        }
        Some(format!("memo: \"{s}\" (before+after)"))
    } else if let Some(obj) = v.as_object() {
        let before = obj.get("before").and_then(|x| x.as_str());
        let after = obj.get("after").and_then(|x| x.as_str());
        match (before, after) {
            (Some(b), Some(a)) => Some(format!("memo: before \"{b}\" / after \"{a}\"")),
            (Some(b), None) => Some(format!("memo: before \"{b}\"")),
            (None, Some(a)) => Some(format!("memo: after \"{a}\"")),
            (None, None) => None,
        }
    } else {
        None
    }
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        // `memo:` is the sole discriminant — string shorthand
        // or `{before, after}` map. No inner/outer constraints:
        // memo publication is independent of every other
        // wrapper's behaviour; it sees the same wires every
        // wrapper sees and writes to its own atomic.
        owned_fields: &["memo"],
        triggers,
        requires_inner: &[super::traverse::NAME],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Op-wrapper whose only side effect is to publish a short
/// human-visible string to the activity's `memo` ArcSwap.
///
/// Two templates are accepted:
///
/// - `before`: rendered + stored *before* the inner op runs.
///   Useful for "now compacting {table}" style state — reads
///   workload params / cycle wires that exist pre-execution.
/// - `after`: rendered + stored *after* the inner op returns Ok.
///   Lets the next-rendered memo reflect the post-op state.
///
/// Either or both may be present. A shorthand string form
/// (`memo: "doing X"`) is parsed as both-templates-the-same.
///
/// The wrapper is a no-op on inner errors — the result is
/// returned unchanged whether or not memo publication happened.
/// Substitution failures are downgraded to a debug log; we
/// don't fail an otherwise-good op because the memo couldn't
/// render.
pub struct MemoDispenser {
    inner: Arc<dyn OpDispenser>,
    before_template: Option<String>,
    after_template: Option<String>,
    /// Shared atomic owned by the activity (see
    /// `Activity::memo`). Cloned into the wrapper at wrap-time
    /// so writes here are visible to the inline-status thread
    /// and end-of-phase readout context without a separate
    /// channel.
    memo_state: Arc<arc_swap::ArcSwap<String>>,
}

impl MemoDispenser {
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        before_template: Option<String>,
        after_template: Option<String>,
        memo_state: Arc<arc_swap::ArcSwap<String>>,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            before_template,
            after_template,
            memo_state,
        })
    }

    fn publish(&self, template: &str, wires: &dyn crate::wires::WireSource) {
        match crate::wires::substitute_via_wires(template, wires) {
            Ok(rendered) => {
                self.memo_state.store(Arc::new(rendered));
            }
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "memo: substitution failed for '{template}': {e}"
                );
            }
        }
    }
}

impl WrappingDispenser for MemoDispenser {}

impl OpDispenser for MemoDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if let Some(t) = &self.before_template {
                self.publish(t, ctx.wires);
            }
            let result = self.inner.execute(cycle, ctx).await?;
            if let Some(t) = &self.after_template {
                self.publish(t, ctx.wires);
            }
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
    use crate::adapter::{AdapterError, ExecutionError, OpResult, ResultBody};
    use crate::fixture::{ExecCtx, ResolvedPulls};

    #[derive(Debug)]
    struct ResultDispBody {
        value: serde_json::Value,
        count: u64,
    }
    impl ResultBody for ResultDispBody {
        fn to_json(&self) -> serde_json::Value {
            self.value.clone()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn element_count(&self) -> u64 {
            self.count
        }
    }

    /// A canned-result inner dispenser. `body` controls the
    /// successful path; `error` short-circuits to `ExecutionError`.
    struct FakeInner {
        body: Option<ResultDispBody>,
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
                    body: self.body.as_ref().map(|b| {
                        Box::new(ResultDispBody {
                            value: b.value.clone(),
                            count: b.count,
                        }) as Box<dyn ResultBody>
                    }),
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
    async fn memo_wrapper_publishes_before_and_after() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::new()));
        let inner = Arc::new(FakeInner {
            body: None,
            error: None,
        });
        let dispenser = MemoDispenser::wrap(
            inner,
            Some("before-state".into()),
            Some("after-state".into()),
            memo.clone(),
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let _ = dispenser.execute(0, &ctx).await.expect("inner ok");
        assert_eq!(memo.load().as_str(), "after-state");
    }

    #[tokio::test]
    async fn memo_wrapper_only_before_when_after_unset() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::new()));
        let inner = Arc::new(FakeInner {
            body: None,
            error: None,
        });
        let dispenser = MemoDispenser::wrap(inner, Some("ready".into()), None, memo.clone());
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let _ = dispenser.execute(0, &ctx).await.expect("inner ok");
        assert_eq!(memo.load().as_str(), "ready");
    }

    #[tokio::test]
    async fn memo_wrapper_does_not_run_after_on_inner_error() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::new()));
        let inner = Arc::new(FakeInner {
            body: None,
            error: Some("boom"),
        });
        let dispenser = MemoDispenser::wrap(
            inner,
            Some("attempting".into()),
            Some("finished".into()),
            memo.clone(),
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let res = dispenser.execute(0, &ctx).await;
        assert!(res.is_err());
        assert_eq!(
            memo.load().as_str(),
            "attempting",
            "after-template must not run on inner error"
        );
    }
}
