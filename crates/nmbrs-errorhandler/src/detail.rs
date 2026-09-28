// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Error detail: the immutable value passed through the handler chain.

/// Whether an operation should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    DoRetry,
    DoNotRetry,
    Unset,
}

/// The error detail passed through and transformed by the handler chain.
///
/// Immutable — handlers produce new instances via `with_*` methods.
#[derive(Debug, Clone)]
pub struct ErrorDetail {
    /// Classification name (typically the error type name).
    pub name: String,
    /// Whether the operation should be retried.
    pub retry: Retry,
    /// Result code for the operation (0 = OK, nonzero = error).
    pub result_code: i32,
    /// Whether execution should stop after this error.
    pub should_stop: bool,
}

impl ErrorDetail {
    pub fn ok() -> Self {
        Self {
            name: "OK".into(),
            retry: Retry::Unset,
            result_code: 0,
            should_stop: false,
        }
    }

    pub fn non_retryable(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            retry: Retry::DoNotRetry,
            result_code: 127,
            should_stop: false,
        }
    }

    pub fn retryable(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            retry: Retry::DoRetry,
            result_code: 127,
            should_stop: false,
        }
    }

    pub fn unknown(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            retry: Retry::Unset,
            result_code: 127,
            should_stop: false,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retry == Retry::DoRetry
    }

    pub fn with_retryable(mut self) -> Self {
        self.retry = Retry::DoRetry;
        self
    }

    pub fn with_not_retryable(mut self) -> Self {
        self.retry = Retry::DoNotRetry;
        self
    }

    pub fn with_result_code(mut self, code: i32) -> Self {
        self.result_code = code;
        self
    }

    pub fn with_stop(mut self) -> Self {
        self.should_stop = true;
        self
    }
}
