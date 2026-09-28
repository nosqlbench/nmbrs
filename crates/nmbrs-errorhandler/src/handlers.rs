// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Built-in error handler implementations.

use std::sync::atomic::AtomicPtr;

/// Global log function pointer. Set by the runtime to route
/// error handler messages through the observer instead of stderr.
/// Default: write to stderr.
static LOG_FN: AtomicPtr<()> = AtomicPtr::new(default_log as *mut ());

type LogFn = fn(&str);

fn default_log(msg: &str) {
    eprintln!("{msg}");
}

/// Set the global log function for error handlers.
pub fn set_log_fn(f: LogFn) {
    LOG_FN.store(f as *mut (), std::sync::atomic::Ordering::Release);
}

fn emit(msg: &str) {
    let ptr = LOG_FN.load(std::sync::atomic::Ordering::Acquire);
    let f: LogFn = unsafe { std::mem::transmute(ptr) };
    f(msg);
}

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::detail::ErrorDetail;
use crate::handler::ErrorHandler;

/// Signal that execution should stop after this error.
/// Does NOT print — that's `warn`'s job. Use `warn,stop` to both log and halt.
pub struct StopHandler;

impl ErrorHandler for StopHandler {
    fn handle(
        &self,
        _name: &str,
        _error_msg: &str,
        _cycle: u64,
        _duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail {
        detail.with_stop()
    }
}

/// Log a warning and pass through.
pub struct WarnHandler;

impl ErrorHandler for WarnHandler {
    fn handle(
        &self,
        name: &str,
        error_msg: &str,
        cycle: u64,
        _duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail {
        emit(&format!(
            "WARN error at cycle {cycle}: [{name}] {error_msg}"
        ));
        detail
    }
}

/// Log an error and pass through.
pub struct ErrorLogHandler;

impl ErrorHandler for ErrorLogHandler {
    fn handle(
        &self,
        name: &str,
        error_msg: &str,
        cycle: u64,
        _duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail {
        emit(&format!("ERROR at cycle {cycle}: [{name}] {error_msg}"));
        detail
    }
}

/// Silently pass through (no-op).
pub struct IgnoreHandler;

impl ErrorHandler for IgnoreHandler {
    fn handle(
        &self,
        _name: &str,
        _error_msg: &str,
        _cycle: u64,
        _duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail {
        detail
    }
}

/// Mark the error as retryable.
pub struct RetryHandler;

impl ErrorHandler for RetryHandler {
    fn handle(
        &self,
        _name: &str,
        _error_msg: &str,
        _cycle: u64,
        _duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail {
        detail.with_retryable()
    }
}

/// Count errors by type name.
pub struct CounterHandler {
    counts: Mutex<HashMap<String, AtomicU64>>,
}

impl Default for CounterHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl CounterHandler {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Get the current count for a specific error name.
    #[allow(dead_code)]
    pub fn get_count(&self, name: &str) -> u64 {
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts
            .get(name)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get all error counts.
    #[allow(dead_code)]
    pub fn all_counts(&self) -> HashMap<String, u64> {
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect()
    }
}

impl ErrorHandler for CounterHandler {
    fn handle(
        &self,
        name: &str,
        _error_msg: &str,
        _cycle: u64,
        _duration_nanos: u64,
        detail: ErrorDetail,
    ) -> ErrorDetail {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts
            .entry(name.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        detail
    }
}

/// Look up a built-in handler by name.
pub fn builtin_handler(name: &str) -> Option<Box<dyn ErrorHandler>> {
    match name {
        "stop" => Some(Box::new(StopHandler)),
        "warn" => Some(Box::new(WarnHandler)),
        "error" => Some(Box::new(ErrorLogHandler)),
        "ignore" => Some(Box::new(IgnoreHandler)),
        "retry" => Some(Box::new(RetryHandler)),
        "counter" | "count" => Some(Box::new(CounterHandler::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_handler_sets_retryable() {
        let h = RetryHandler;
        let detail = ErrorDetail::non_retryable("test");
        let result = h.handle("test", "msg", 0, 0, detail);
        assert!(result.is_retryable());
    }

    #[test]
    fn ignore_handler_passes_through() {
        let h = IgnoreHandler;
        let detail = ErrorDetail::non_retryable("test");
        let result = h.handle("test", "msg", 0, 0, detail);
        assert!(!result.is_retryable());
        assert_eq!(result.result_code, 127);
    }

    #[test]
    fn counter_handler_counts() {
        let h = CounterHandler::new();
        let detail = ErrorDetail::non_retryable("TimeoutError");
        h.handle("TimeoutError", "timed out", 1, 0, detail.clone());
        h.handle("TimeoutError", "timed out", 2, 0, detail.clone());
        h.handle("OtherError", "other", 3, 0, detail);
        assert_eq!(h.get_count("TimeoutError"), 2);
        assert_eq!(h.get_count("OtherError"), 1);
        assert_eq!(h.get_count("Missing"), 0);
    }

    #[test]
    fn stop_handler_sets_should_stop() {
        let h = StopHandler;
        let detail = ErrorDetail::non_retryable("test");
        let result = h.handle("test", "boom", 42, 0, detail);
        assert!(
            result.should_stop,
            "stop handler should set should_stop flag"
        );
    }

    #[test]
    fn builtin_lookup() {
        assert!(builtin_handler("stop").is_some());
        assert!(builtin_handler("warn").is_some());
        assert!(builtin_handler("error").is_some());
        assert!(builtin_handler("ignore").is_some());
        assert!(builtin_handler("retry").is_some());
        assert!(builtin_handler("counter").is_some());
        assert!(builtin_handler("count").is_some());
        assert!(builtin_handler("bogus").is_none());
    }
}
