// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Shared shadow-terminal PTY drive helpers for the display
//! harness suites.
//!
//! Every PTY suite used to hand-roll its own drain/wait loop, and
//! two of the local variants carried a per-pass
//! `tokio::time::timeout(400ms, render_all_output())` guard. That
//! guard was the root of the suites' hang flake:
//!
//! - `render_all_output()` appends recv'd bytes to the shadow
//!   terminal's `accumulated_pty_output` and only CLEARS the
//!   buffer after a full uncancelled pass. Cancelling it mid-pass
//!   (wezterm `advance_bytes` on a debug build can exceed 400ms)
//!   leaves the buffer intact, so the next pass re-advances the
//!   same bytes plus new ones — each pass is slower than the
//!   last, every pass gets cancelled, and the screen never
//!   updates again. The wait loop then rides its full deadline
//!   even though the child completed long ago.
//! - The deadline `panic!` made it worse: unwinding drops the
//!   `SteppableTerminal`, whose kill path only SIGKILLs the child
//!   from an async task — which the dying runtime never polls.
//!   The child lives on, shadow-terminal's `spawn_blocking`
//!   child-wait never returns, and tokio's `Runtime::drop` joins
//!   blocking tasks unconditionally: the whole test binary hangs.
//!
//! The helpers here therefore (a) never cancel a render pass —
//! `render_all_output` is `try_recv`-bounded, so it terminates on
//! its own whenever the pipeline momentarily empties — and (b) on
//! deadline, kill the child and give the runtime a beat to
//! deliver the SIGKILL BEFORE panicking, so teardown can never
//! wedge behind a live child.

// Each consuming test crate uses its own subset of these helpers;
// the unused remainder is expected per crate.
#![allow(dead_code)]

use std::time::Duration;

use shadow_terminal::steppable_terminal::SteppableTerminal;

/// One full drain-and-render pass, uncancelled. Bounded by
/// construction: `render_all_output` loops on `try_recv` and
/// returns at the first empty poll of the output channel.
pub async fn drain(stepper: &mut SteppableTerminal) {
    let _ = stepper.render_all_output().await;
}

/// Kill the child, give the runtime a beat to actually deliver
/// the SIGKILL (the kill is executed by an async task that must
/// get polled), then panic with the last screen. Keeping the
/// child from outliving the test is what keeps the runtime's
/// blocking-pool join from wedging the whole test binary.
pub async fn kill_and_panic(stepper: &mut SteppableTerminal, msg: &str) -> ! {
    let dump = stepper.screen_as_string().unwrap_or_default();
    let _ = stepper.kill();
    tokio::time::sleep(Duration::from_millis(250)).await;
    panic!("{msg}; last screen:\n{dump}");
}

/// Step the emulator until `pred` accepts the screen, or kill the
/// child and panic at the deadline. `what` names the awaited
/// condition in the panic message.
pub async fn wait_until(
    stepper: &mut SteppableTerminal,
    pred: impl Fn(&str) -> bool,
    timeout: Duration,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        drain(stepper).await;
        if let Ok(s) = stepper.screen_as_string()
            && pred(&s)
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            kill_and_panic(stepper, &format!("timed out waiting for {what}")).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Step the emulator until `needle` appears on the screen.
pub async fn wait_for(stepper: &mut SteppableTerminal, needle: &str, timeout: Duration) {
    wait_until(
        stepper,
        |s| s.contains(needle),
        timeout,
        &format!("{needle:?}"),
    )
    .await;
}

/// Drain a few more passes so the final screen is complete.
pub async fn settle(stepper: &mut SteppableTerminal) {
    for _ in 0..5 {
        drain(stepper).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
