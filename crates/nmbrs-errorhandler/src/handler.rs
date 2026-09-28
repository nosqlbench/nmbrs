// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! ErrorHandler trait: the composable unit in the handler chain.

use crate::detail::ErrorDetail;

/// A single error handler in the chain.
///
/// Each handler can:
/// - Perform side effects (logging, counting, metering)
/// - Transform the `ErrorDetail` (mark as retryable, change result code)
/// - Halt execution (by panicking or returning a stop signal)
///
/// Handlers are composable: the chain passes `ErrorDetail` through
/// each handler in sequence.
pub trait ErrorHandler: Send + Sync {
    /// Handle an error, returning a potentially transformed ErrorDetail.
    ///
    /// - `name`: error classification name (type name or custom)
    /// - `error_msg`: the error message
    /// - `cycle`: the cycle number that was executing
    /// - `duration_nanos`: how long into the operation the error occurred
    /// - `detail`: the current ErrorDetail from upstream handlers
    fn handle(
        &self,
        name: &str,
        error_msg: &str,
        cycle: u64,
        duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail;
}
