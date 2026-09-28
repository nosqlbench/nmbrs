// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Asynchronous log file sink.
//!
//! See SRD-02 §"Display and Diagnostic Decoupling" for the
//! design tenet. Producers (anyone calling `diag!()` /
//! `observer::log()`) `try_send` a fully-formatted line into a
//! bounded channel and return immediately. A dedicated
//! `log-sink` OS thread is the only writer to the file —
//! syscalls, locking, fsync stalls all happen there, never on
//! a tokio worker.
//!
//! Overflow policy: bounded channel + `try_send`. If the sink
//! falls behind (slow disk, full disk, NFS hang), producers
//! drop the line and bump a `dropped_count` counter.
//! Diagnostics never block the runtime; the dropped count is
//! visible through the inspector endpoint and the post-run
//! summary so the operator knows when log loss occurred.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;

/// Capacity of the bounded log channel. Sized for short bursts
/// (every fiber emits one line at end-of-phase, plus periodic
/// drain progress) without overusing memory: 4096 lines × ~256
/// bytes ≈ 1 MB worst case in the queue.
const LOG_CHANNEL_CAPACITY: usize = 4096;

/// The single global log sink — set once via [`init`] when the
/// session directory is known.
static GLOBAL_LOG_SINK: OnceLock<LogSink> = OnceLock::new();

/// Producer side of the async log sink.
pub struct LogSink {
    /// Bounded sender. Producers `try_send`; on overflow they
    /// drop the line and bump [`Self::dropped_count`]. Never
    /// blocks.
    sender: mpsc::SyncSender<Vec<u8>>,
    /// Lines dropped because the sink couldn't keep up. Visible
    /// through the inspector and reported once at shutdown.
    dropped_count: AtomicU64,
}

impl LogSink {
    /// Try to enqueue a fully-formatted log line. Never blocks.
    /// Returns `Ok(())` if accepted, `Err(())` if dropped.
    // reason: the only failure is "dropped"; there is no error payload to
    // carry, and callers branch on Ok/Err alone — a richer error type would
    // be noise.
    #[allow(clippy::result_unit_err)]
    pub fn try_send(&self, line: Vec<u8>) -> Result<(), ()> {
        match self.sender.try_send(line) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
                Err(())
            }
        }
    }

    /// Count of lines dropped since startup. Useful as a health
    /// signal in the inspector and at shutdown.
    pub fn dropped_count(&self) -> u64 {
        self.dropped_count.load(Ordering::Relaxed)
    }
}

/// Initialize the global log sink with a target file. Called
/// once by the runner after the session directory exists.
/// Silently no-ops on a second call — the first session wins,
/// matching the previous behavior of `set_log_file`.
pub fn init(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(LOG_CHANNEL_CAPACITY);
    spawn_writer_thread(file, rx);
    let _ = GLOBAL_LOG_SINK.set(LogSink {
        sender: tx,
        dropped_count: AtomicU64::new(0),
    });
    Ok(())
}

/// Borrow the global log sink, if initialized. Used by the
/// `log()` hot path and by the inspector to report the dropped
/// count.
pub fn global() -> Option<&'static LogSink> {
    GLOBAL_LOG_SINK.get()
}

fn spawn_writer_thread(mut file: File, rx: mpsc::Receiver<Vec<u8>>) {
    thread::Builder::new()
        .name("log-sink".into())
        .spawn(move || {
            // recv() blocks the dedicated writer thread when
            // the channel is empty — fine, this is not a tokio
            // worker. SRD-02 §"No Blocking Primitives in Async
            // Contexts" only forbids blocking *inside* tokio.
            while let Ok(buf) = rx.recv() {
                // Best-effort write. A failed write means the
                // file system is unhealthy; we keep draining so
                // upstream producers still see `try_send`
                // success and the runtime keeps moving. Failed
                // writes are silent today; an explicit
                // last-write-error counter is a follow-up if
                // needed.
                let _ = file.write_all(&buf);
            }
            // Channel closed (all senders dropped). Flush
            // pending writes to disk before the thread exits.
            let _ = file.flush();
            let _ = file.sync_all();
        })
        .expect("spawn log-sink thread");
}
