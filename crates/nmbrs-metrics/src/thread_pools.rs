// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-102 — Named physical thread pools.
//!
//! A process-wide registry of **named physical thread pools**: sets of real OS
//! threads (`std::thread`, not tokio tasks) dedicated to one role, each with a
//! scheduling policy (priority + optional CPU affinity). Realtime-sensitive,
//! schedule-keeping work (the cadence scheduler) runs on the `timing` pool so
//! it is never queued behind the async worker pool.
//!
//! Placed in `nmbrs-metrics` (layer 2, per SRD-05) so both `nmbrs-metrics` (the
//! scheduler) and `nmbrs-runtime` (which sizes the `workers` runtime) can read
//! it without an upward dependency edge.
//!
//! Config resolves from env + core count with reasonable defaults (SRD-102 §4);
//! CLI-flag overlay lives in the `nmbrs` binary. Unknown pool/policy names are
//! hard errors — never silently ignored.

use std::sync::OnceLock;
use std::thread::JoinHandle;

/// Scheduling class applied to a pool's threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedPolicy {
    /// `SCHED_RR` realtime (default for `timing`). Round-robin so a busy
    /// realtime thread can't hard-starve a core the way `Fifo` can.
    Rr,
    /// `SCHED_FIFO` realtime — tightest, but a runaway thread starves the core.
    Fifo,
    /// A `nice` bump (no privileges required).
    Nice,
    /// Plain (default) scheduling.
    None,
}

impl SchedPolicy {
    /// Parse a policy token (`rr`/`fifo`/`nice`/`none`). `Err` for anything
    /// else — never silently coerced (cf. the `max_batch_size` no-op).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rr" => Ok(Self::Rr),
            "fifo" => Ok(Self::Fifo),
            "nice" => Ok(Self::Nice),
            "none" | "plain" | "" => Ok(Self::None),
            other => Err(format!(
                "unknown thread scheduling policy '{other}' (use rr|fifo|nice|none)"
            )),
        }
    }
}

/// CPU affinity for a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Reserve + pin a core (chosen by the registry from the top of the range).
    Auto,
    /// Pin to a specific core index.
    Core(usize),
    /// No affinity.
    Off,
}

impl Affinity {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "auto" => Ok(Self::Auto),
            "off" | "none" | "" => Ok(Self::Off),
            _ => t
                .parse::<usize>()
                .map(Self::Core)
                .map_err(|_| format!("unknown thread affinity '{s}' (use auto|off|<core>)")),
        }
    }
}

/// Per-pool spec.
#[derive(Debug, Clone, Copy)]
pub struct PoolSpec {
    pub threads: usize,
    pub sched: SchedPolicy,
    pub pin: Affinity,
}

/// Raw (unparsed) CLI `--threads.*` override values, collected by the `nmbrs`
/// binary from the process args before dispatch and handed to
/// [`ThreadPoolConfig::resolve_with_cli`]. Held as `Option<String>` (not yet
/// parsed) so the parse + not-silent error contract stays in one place here,
/// identical to the `NMBRS_THREADS_*` env path. Each field mirrors one env var:
/// `timing`↔`NMBRS_THREADS_TIMING`, `io`↔`NMBRS_THREADS_IO`,
/// `workers`↔`NMBRS_THREADS_WORKERS`, `timing_sched`↔`NMBRS_THREADS_TIMING_SCHED`,
/// `timing_pin`↔`NMBRS_THREADS_TIMING_PIN`.
#[derive(Debug, Default, Clone)]
pub struct CliThreadOverrides {
    /// `--threads.timing=N`
    pub timing: Option<String>,
    /// `--threads.io=N`
    pub io: Option<String>,
    /// `--threads.workers=N`
    pub workers: Option<String>,
    /// `--threads.timing.sched=rr|fifo|nice|none`
    pub timing_sched: Option<String>,
    /// `--threads.timing.pin=auto|off|<core>`
    pub timing_pin: Option<String>,
}

/// Resolved, immutable process-wide thread-pool configuration.
#[derive(Debug, Clone, Copy)]
pub struct ThreadPoolConfig {
    /// Low-jitter periodic dispatch (the cadence scheduler).
    pub timing: PoolSpec,
    /// Offloaded reporter / report I/O.
    pub io: PoolSpec,
    /// The tokio async runtime worker-thread count (consumed by the runtime
    /// builder in `nmbrs-runtime`; the registry does not own that runtime).
    pub workers: usize,
}

impl ThreadPoolConfig {
    /// Reasonable defaults derived from the core count (SRD-102 §3): 1 `timing`
    /// thread (RR + auto-pin), 2 `io` threads, `workers = cores − reserved`.
    /// On < 3 cores the reservation degrades (workers gets at least 1; the
    /// `timing` pin/RR still applies but shares a core — logged at spawn).
    pub fn defaults() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        // Reserve one core for `timing` when we can afford it.
        let reserved = if cores >= 3 { 1 } else { 0 };
        let workers = cores.saturating_sub(reserved).max(1);
        Self {
            timing: PoolSpec {
                threads: 1,
                sched: SchedPolicy::Rr,
                pin: Affinity::Auto,
            },
            io: PoolSpec {
                threads: 2,
                sched: SchedPolicy::None,
                pin: Affinity::Off,
            },
            workers,
        }
    }

    /// Overlay `NMBRS_THREADS_*` env vars onto the defaults. Returns `Err` on a
    /// malformed value (never silently ignored). The `nmbrs` binary layers CLI
    /// flags on top of this before calling [`init`].
    pub fn resolve() -> Result<Self, String> {
        let mut cfg = Self::defaults();
        let env_usize = |k: &str| -> Result<Option<usize>, String> {
            match std::env::var(k) {
                Ok(v) => v
                    .trim()
                    .parse::<usize>()
                    .map(Some)
                    .map_err(|_| format!("{k}='{v}' is not a thread count")),
                Err(_) => Ok(None),
            }
        };
        if let Some(n) = env_usize("NMBRS_THREADS_TIMING")? {
            cfg.timing.threads = n;
        }
        if let Some(n) = env_usize("NMBRS_THREADS_IO")? {
            cfg.io.threads = n;
        }
        if let Some(n) = env_usize("NMBRS_THREADS_WORKERS")? {
            cfg.workers = n.max(1);
        }
        if let Ok(v) = std::env::var("NMBRS_THREADS_TIMING_SCHED") {
            cfg.timing.sched = SchedPolicy::parse(&v)?;
        }
        if let Ok(v) = std::env::var("NMBRS_THREADS_TIMING_PIN") {
            cfg.timing.pin = Affinity::parse(&v)?;
        }
        Ok(cfg)
    }

    /// Resolve with CLI `--threads.*` flags layered on top of env + defaults,
    /// giving the **CLI > env > defaults** precedence of SRD-102 §4. Parsing
    /// lives here (not in the `nmbrs` binary) so a malformed CLI value is the
    /// SAME hard error as the env path — via the same [`SchedPolicy::parse`] /
    /// [`Affinity::parse`] and thread-count parsing, never silently coerced.
    pub fn resolve_with_cli(cli: &CliThreadOverrides) -> Result<Self, String> {
        let mut cfg = Self::resolve()?;
        let usize_flag = |v: &str, flag: &str| -> Result<usize, String> {
            v.trim()
                .parse::<usize>()
                .map_err(|_| format!("{flag}='{v}' is not a thread count"))
        };
        if let Some(v) = &cli.timing {
            cfg.timing.threads = usize_flag(v, "--threads.timing")?;
        }
        if let Some(v) = &cli.io {
            cfg.io.threads = usize_flag(v, "--threads.io")?;
        }
        if let Some(v) = &cli.workers {
            cfg.workers = usize_flag(v, "--threads.workers")?.max(1);
        }
        if let Some(v) = &cli.timing_sched {
            cfg.timing.sched = SchedPolicy::parse(v)?;
        }
        if let Some(v) = &cli.timing_pin {
            cfg.timing.pin = Affinity::parse(v)?;
        }
        Ok(cfg)
    }

    fn spec(&self, pool: &str) -> Result<PoolSpec, String> {
        match pool {
            "timing" => Ok(self.timing),
            "io" => Ok(self.io),
            other => Err(format!(
                "unknown thread pool '{other}' (named pools: timing, io, workers)"
            )),
        }
    }
}

/// Process-wide registry. Owns the resolved config and applies each pool's
/// scheduling policy at thread spawn.
pub struct ThreadPools {
    config: ThreadPoolConfig,
    /// Top core index available for `Auto` pinning (cores-1); handed out
    /// downward per pinned pool.
    top_core: usize,
}

static GLOBAL: OnceLock<ThreadPools> = OnceLock::new();

/// Install the process-wide registry from resolved config. Idempotent-safe:
/// a second call is ignored (the first wins), matching start-time immutability.
pub fn init(config: ThreadPoolConfig) {
    let _ = GLOBAL.set(ThreadPools::new(config));
}

/// The process-wide registry. Lazily initialised with [`ThreadPoolConfig::defaults`]
/// if [`init`] was never called (library/test use without explicit setup).
pub fn global() -> &'static ThreadPools {
    GLOBAL.get_or_init(|| ThreadPools::new(ThreadPoolConfig::defaults()))
}

impl ThreadPools {
    fn new(config: ThreadPoolConfig) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            config,
            top_core: cores.saturating_sub(1),
        }
    }

    pub fn config(&self) -> &ThreadPoolConfig {
        &self.config
    }

    /// Resolve a pool's `Auto` affinity to a concrete core (the top core), else
    /// pass through `Core`/`Off`.
    fn resolve_pin(&self, spec: &PoolSpec) -> Option<usize> {
        match spec.pin {
            Affinity::Auto => Some(self.top_core),
            Affinity::Core(c) => Some(c),
            Affinity::Off => None,
        }
    }

    /// Spawn a thread on a named pool with that pool's scheduling policy applied
    /// from *inside* the new thread (the syscalls target the calling thread).
    /// The achieved policy is logged once at thread start. `Err` on an unknown
    /// pool name.
    pub fn spawn(
        &self,
        pool: &str,
        name: &str,
        f: impl FnOnce() + Send + 'static,
    ) -> Result<JoinHandle<()>, String> {
        let spec = self.config.spec(pool)?;
        let pin = self.resolve_pin(&spec);
        let sched = spec.sched;
        let pool_owned = pool.to_string();
        let name_owned = name.to_string();
        std::thread::Builder::new()
            .name(format!("{pool}-{name}"))
            .spawn(move || {
                let achieved = apply_policy(sched, pin);
                let line = format!("thread pool '{pool_owned}/{name_owned}': {achieved}");
                if achieved.contains("denied") {
                    crate::diag::warn(&line);
                } else {
                    crate::diag::info(&line);
                }
                f();
            })
            .map_err(|e| format!("spawn thread pool '{pool}/{name}': {e}"))
    }

    /// Convenience for the cadence scheduler.
    pub fn spawn_timing(
        &self,
        name: &str,
        f: impl FnOnce() + Send + 'static,
    ) -> Result<JoinHandle<()>, String> {
        self.spawn("timing", name, f)
    }
}

/// Apply a scheduling policy to the *current* thread, returning a short
/// description of what was actually achieved (for the startup log). Realtime is
/// best-effort: `SCHED_RR`/`FIFO` → `nice` → plain, each degradation visible.
#[cfg(target_os = "linux")]
fn apply_policy(sched: SchedPolicy, pin: Option<usize>) -> String {
    let mut parts: Vec<String> = Vec::new();
    match sched {
        SchedPolicy::Rr | SchedPolicy::Fifo => {
            let policy = if matches!(sched, SchedPolicy::Fifo) {
                libc::SCHED_FIFO
            } else {
                libc::SCHED_RR
            };
            let prio = 10; // mid-range realtime priority (RR range is 1..=99)
            let param = libc::sched_param {
                sched_priority: prio,
            };
            // 0 = the calling thread.
            let rc = unsafe { libc::sched_setscheduler(0, policy, &param) };
            if rc == 0 {
                let name = if policy == libc::SCHED_FIFO {
                    "fifo"
                } else {
                    "rr"
                };
                parts.push(format!("sched={name}(prio {prio})"));
            } else {
                // Realtime denied (no CAP_SYS_NICE) — fall back to nice.
                let rc2 = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -5) };
                if rc2 == 0 {
                    parts.push("sched=nice(-5) [realtime denied]".to_string());
                } else {
                    parts.push("sched=plain [realtime+nice denied]".to_string());
                }
            }
        }
        SchedPolicy::Nice => {
            let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -5) };
            parts.push(if rc == 0 {
                "sched=nice(-5)".to_string()
            } else {
                "sched=plain [nice denied]".to_string()
            });
        }
        SchedPolicy::None => parts.push("sched=plain".to_string()),
    }
    if let Some(core) = pin {
        let rc = unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(core, &mut set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set)
        };
        parts.push(if rc == 0 {
            format!("pin={core}")
        } else {
            format!("pin={core} [denied]")
        });
    }
    parts.join(", ")
}

#[cfg(not(target_os = "linux"))]
fn apply_policy(_sched: SchedPolicy, _pin: Option<usize>) -> String {
    // Isolation (dedicated thread) still holds; policy is a no-op elsewhere.
    "sched=plain [non-linux]".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reserve_a_core_when_affordable() {
        let cfg = ThreadPoolConfig::defaults();
        assert_eq!(cfg.timing.threads, 1);
        assert_eq!(cfg.timing.sched, SchedPolicy::Rr);
        assert!(matches!(cfg.timing.pin, Affinity::Auto));
        assert!(cfg.workers >= 1);
    }

    #[test]
    fn unknown_policy_and_pool_are_hard_errors() {
        assert!(SchedPolicy::parse("bogus").is_err());
        assert!(Affinity::parse("nonsense").is_err());
        assert!(ThreadPoolConfig::defaults().spec("nope").is_err());
    }

    #[test]
    fn policy_and_affinity_parse_roundtrip() {
        assert_eq!(SchedPolicy::parse("RR").unwrap(), SchedPolicy::Rr);
        assert_eq!(SchedPolicy::parse("none").unwrap(), SchedPolicy::None);
        assert_eq!(Affinity::parse("auto").unwrap(), Affinity::Auto);
        assert_eq!(Affinity::parse("3").unwrap(), Affinity::Core(3));
    }

    #[test]
    fn cli_overrides_apply_and_win() {
        // CLI values override whatever env + defaults produced (CLI wins).
        let cli = CliThreadOverrides {
            timing: Some("4".into()),
            io: Some("3".into()),
            workers: Some("7".into()),
            timing_sched: Some("none".into()),
            timing_pin: Some("off".into()),
            ..Default::default()
        };
        let cfg = ThreadPoolConfig::resolve_with_cli(&cli).unwrap();
        assert_eq!(cfg.timing.threads, 4);
        assert_eq!(cfg.io.threads, 3);
        assert_eq!(cfg.workers, 7);
        assert_eq!(cfg.timing.sched, SchedPolicy::None);
        assert_eq!(cfg.timing.pin, Affinity::Off);
    }

    #[test]
    fn cli_override_absent_leaves_resolved_value() {
        // An all-None override is a no-op: same as resolve() (defaults here,
        // since no NMBRS_THREADS_* is set in the unit-test env).
        let cfg = ThreadPoolConfig::resolve_with_cli(&CliThreadOverrides::default()).unwrap();
        assert_eq!(cfg.timing.threads, 1);
        assert_eq!(cfg.timing.sched, SchedPolicy::Rr);
    }

    #[test]
    fn cli_malformed_value_is_a_hard_error() {
        let bad_count = CliThreadOverrides {
            timing: Some("abc".into()),
            ..Default::default()
        };
        let e = ThreadPoolConfig::resolve_with_cli(&bad_count).unwrap_err();
        assert!(
            e.contains("--threads.timing"),
            "message names the flag: {e}"
        );
        assert!(
            e.contains("not a thread count"),
            "message explains why: {e}"
        );

        let bad_sched = CliThreadOverrides {
            timing_sched: Some("bogus".into()),
            ..Default::default()
        };
        assert!(ThreadPoolConfig::resolve_with_cli(&bad_sched).is_err());

        let bad_pin = CliThreadOverrides {
            timing_pin: Some("nonsense".into()),
            ..Default::default()
        };
        assert!(ThreadPoolConfig::resolve_with_cli(&bad_pin).is_err());
    }

    #[test]
    fn spawn_on_timing_runs_the_closure() {
        let pools = ThreadPools::new(ThreadPoolConfig::defaults());
        let (tx, rx) = std::sync::mpsc::channel();
        let h = pools
            .spawn_timing("unit", move || {
                let _ = tx.send(42u8);
            })
            .unwrap();
        assert_eq!(rx.recv().unwrap(), 42);
        h.join().unwrap();
    }
}
