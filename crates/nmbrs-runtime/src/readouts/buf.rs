// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! [`ReadoutBuf`] — the surface-supplied output buffer.
//!
//! Push 1 only needs the [`String`]-backed buffer that the
//! terminal-mode log path uses (ANSI escape sequences land
//! inline as text). Later pushes will add a `Vec<Span>`-backed
//! buffer for the TUI surface; the [`ReadoutBuf`] trait is
//! the single write surface a [`Readout`](crate::Readout)
//! impl sees regardless of which backing store is in use.

use std::fmt::{self, Write as _};

/// Append-only write surface a readout writes its rendered
/// piece into. Implementations decide how to handle styling
/// (ANSI inline vs. typed style runs) and ANSI stripping
/// (TTY vs. non-TTY sinks).
///
/// The trait is intentionally minimal — `write_str` is
/// enough for the Push 1 built-in. Later pushes will extend
/// it with style-run pushes (`push_styled`) once the colour
/// / style sub-language lands; until then, readouts emit
/// ANSI escapes as part of `write_str` payloads when they
/// want styling, and the buffer's own ANSI policy decides
/// whether to keep or strip.
pub trait ReadoutBuf {
    /// Append the given UTF-8 text. Must not allocate
    /// independently of the underlying storage's growth.
    fn write_str(&mut self, s: &str) -> fmt::Result;

    /// Hint about how many more bytes will be appended.
    /// Implementations may use this to reserve capacity.
    /// Optional — default no-op.
    fn reserve(&mut self, _additional: usize) {}
}

/// `String`-backed buffer. Used by the terminal-mode log
/// surface, which routes through `nmbrs-runtime::observer::log`
/// and ultimately `eprint!`. ANSI escape sequences pass
/// through unchanged; the surface above strips them when the
/// destination is a non-TTY pipe.
pub struct StringBuf<'a> {
    inner: &'a mut String,
}

impl<'a> StringBuf<'a> {
    pub fn new(inner: &'a mut String) -> Self {
        Self { inner }
    }
}

impl ReadoutBuf for StringBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.inner.write_str(s)
    }

    fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
    }
}

/// Convenience: render a readout into a freshly allocated
/// `String`. Used by the in-process tests and the
/// `crate::diag!` bridge in Push 1; later pushes will route
/// directly through a borrowed buffer instead.
pub fn render_to_string<F>(estimated_size: usize, render: F) -> String
where
    F: FnOnce(&mut StringBuf<'_>) -> usize,
{
    let mut s = String::with_capacity(estimated_size);
    {
        let mut buf = StringBuf::new(&mut s);
        let _ = render(&mut buf);
    }
    s
}
