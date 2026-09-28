// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Unix daemonization and web instance discovery for `nmbrs web`.
//!
//! When `nmbrs web` starts, it writes a `.nmbrs-web.json` anchor file
//! in the working directory recording the host, port, and PID.
//! When `nmbrs run` starts from the same directory, it discovers the
//! anchor and auto-configures metrics push.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli;

/// Entry point for `nmbrs web` — daemon lifecycle, bind, and serve.
pub fn web_command(args: &[String]) {
    // Handle --stop: kill a running daemon
    if args.iter().any(|a| a == "--stop") {
        match stop_daemon() {
            Ok(()) => {}
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Handle --restart: stop the old daemon, re-launch with its saved args.
    // Falls back to process-table scan if no anchor file exists.
    // If nothing is found at all, starts fresh.
    if args.iter().any(|a| a == "--restart") {
        if let Some(anchor) = read_anchor() {
            // Anchor file exists — use it for a clean restart.
            let _ = stop_daemon();
            if !anchor.args.is_empty() {
                let exe = std::env::current_exe().unwrap_or_else(|_| "nmbrs".into());
                eprintln!(
                    "nmbrs web: restarting with: {} {}",
                    exe.display(),
                    anchor.args.join(" ")
                );
                let status = std::process::Command::new(&exe)
                    .args(&anchor.args)
                    .status()
                    .unwrap_or_else(|e| {
                        eprintln!("error: failed to restart: {e}");
                        std::process::exit(1);
                    });
                std::process::exit(status.code().unwrap_or(1));
            }
            eprintln!("nmbrs web: anchor has no saved args, starting with defaults");
        } else {
            // No anchor — scan the process table for orphaned nmbrs web processes.
            let procs = find_nmbrs_web_processes();
            if procs.is_empty() {
                eprintln!("nmbrs web: no running instance found, starting fresh");
            } else {
                eprintln!(
                    "nmbrs web: no anchor file, but found {} running nmbrs web process(es):",
                    procs.len()
                );
                for p in &procs {
                    eprintln!("  pid {} — {}", p.pid, p.cmdline);
                }
                if confirm_prompt("Kill these and start fresh?") {
                    for p in &procs {
                        match kill_pid(p.pid) {
                            Ok(()) => eprintln!("  stopped pid {}", p.pid),
                            Err(e) => eprintln!("  warning: {e}"),
                        }
                    }
                    // Clean up any leftover PID/anchor files.
                    let _ = fs::remove_file(pid_file_path());
                    remove_anchor();
                } else {
                    eprintln!("nmbrs web: aborted");
                    return;
                }
            }
        }
    }

    // Reject unrecognized --flags. Warning-and-proceed silently ran the
    // daemon on defaults after a typo; a bind/port mistake is exactly the
    // input that must not degrade quietly.
    let known_flags = ["--daemon", "--stop", "--restart", "--bind", "--port"];
    for a in args.iter().filter(|a| a.starts_with("--")) {
        let key = a.split('=').next().unwrap_or(a);
        if !known_flags.contains(&key) {
            eprintln!(
                "error: unrecognized option '{a}' (known: --daemon, --stop, --restart, --bind, --port)"
            );
            std::process::exit(2);
        }
    }

    // `--bind`/`--port` accept both spellings the spec advertises
    // (`--bind <addr>` and `--bind=<addr>`), plus the bare kv forms.
    let space_value = |flag: &str| -> Option<&str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    let bind_raw = args
        .iter()
        .find_map(|a| {
            a.strip_prefix("bind=")
                .or_else(|| a.strip_prefix("--bind="))
        })
        .or_else(|| space_value("--bind"))
        .unwrap_or("127.0.0.1");
    let port_raw = args
        .iter()
        .find_map(|a| {
            a.strip_prefix("port=")
                .or_else(|| a.strip_prefix("--port="))
        })
        .or_else(|| space_value("--port"));

    // Parse bind flexibly: accept bare IP, host:port, or full URL
    let (bind, port) = cli::parse_bind_address(bind_raw, port_raw);
    let addr: SocketAddr = format!("{bind}:{port}").parse().unwrap_or_else(|e| {
        eprintln!("error: invalid bind address '{bind}:{port}': {e}");
        std::process::exit(1);
    });

    // Clean up stale anchor if the recorded PID is dead.
    cleanup_stale_anchor();

    // Check if the port is already in use before attempting to bind.
    if let Err(msg) = check_port_available(&addr) {
        eprintln!("error: {msg}");
        std::process::exit(1);
    }

    // Handle --daemon: fork to background
    if args.iter().any(|a| a == "--daemon") {
        eprintln!("nmbrs web: daemonizing on {addr}...");
        daemonize().unwrap_or_else(|e| {
            eprintln!("error: failed to daemonize: {e}");
            std::process::exit(1);
        });
    }

    // Write anchor file so `nmbrs run` in this directory auto-discovers us.
    // Save the full "web ..." args (excluding --restart) for --restart.
    let saved_args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--restart")
        .collect();
    write_anchor(&addr, &saved_args);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let broadcast = nmbrs_web::ws::MetricsBroadcast::new(16);
        if let Err(e) = nmbrs_web::server::serve_with(addr, broadcast).await {
            eprintln!("error: web server failed: {e}");
        }
    });

    // Clean up on exit.
    let _ = fs::remove_file(pid_file_path());
    remove_anchor();
}

/// Name of the anchor file written to the working directory.
const ANCHOR_FILE: &str = ".nmbrs-web.json";

/// Anchor describing a running `nmbrs web` instance.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebAnchor {
    pub host: String,
    pub port: u16,
    pub pid: u32,
    /// Original CLI args used to start the daemon, for `--restart`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl WebAnchor {
    /// The OpenMetrics push URL for this instance.
    #[allow(dead_code)]
    pub fn push_url(&self) -> String {
        format!(
            "http://{}:{}/api/v1/import/prometheus",
            self.host, self.port
        )
    }
}

/// Path to the PID file for the daemon (used by `--stop`).
pub fn pid_file_path() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("nmbrs-web.pid")
}

/// Path to the anchor file in the current working directory.
fn anchor_path() -> PathBuf {
    PathBuf::from(ANCHOR_FILE)
}

/// Write the anchor file for a running web instance.
pub fn write_anchor(addr: &SocketAddr, args: &[String]) {
    // When bound to 0.0.0.0 or ::, use localhost for the push URL.
    let host = match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "::1".to_string(),
        other => other.to_string(),
    };
    let anchor = WebAnchor {
        host,
        port: addr.port(),
        pid: std::process::id(),
        args: args.to_vec(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&anchor)
        && let Err(e) = fs::write(anchor_path(), &json)
    {
        eprintln!("warning: failed to write daemon anchor file: {e}");
    }
}

/// Remove the anchor file on shutdown.
pub fn remove_anchor() {
    let _ = fs::remove_file(anchor_path());
}

/// Read the anchor file, if it exists.
pub fn read_anchor() -> Option<WebAnchor> {
    let content = fs::read_to_string(anchor_path()).ok()?;
    serde_json::from_str(&content).ok()
}

/// Minimal kernel32 surface for process liveness/termination —
/// the Windows stand-ins for `kill(pid, 0)` and SIGTERM/SIGKILL.
/// Hand-declared instead of pulling in `windows-sys` for three
/// imports.
#[cfg(windows)]
mod winproc {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
        fn TerminateProcess(handle: *mut c_void, code: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const STILL_ACTIVE: u32 = 259;

    pub fn pid_alive(pid: u32) -> bool {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE
        }
    }

    pub fn terminate(pid: u32) -> bool {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = TerminateProcess(h, 1);
            CloseHandle(h);
            ok != 0
        }
    }
}

/// `kill(pid, 0)`-style liveness probe.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        winproc::pid_alive(pid)
    }
}

/// Ask a process to terminate. Unix sends SIGTERM (graceful —
/// the daemon's handler gets to clean up); Windows has no
/// SIGTERM analogue for out-of-process signalling, so it's a
/// hard `TerminateProcess`. Returns false when the process
/// couldn't be signalled (usually: already gone).
fn signal_terminate(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) != -1 }
    }
    #[cfg(windows)]
    {
        winproc::terminate(pid)
    }
}

/// Discover a running `nmbrs web` instance from the anchor file.
///
/// Returns `Some(url)` if the anchor exists and the PID is alive.
/// Cleans up stale anchors automatically.
#[allow(dead_code)]
pub fn discover_web_instance() -> Option<String> {
    let path = anchor_path();
    let content = fs::read_to_string(&path).ok()?;
    let anchor: WebAnchor = serde_json::from_str(&content).ok()?;

    // Verify the process is still alive.
    let alive = pid_alive(anchor.pid);
    if !alive {
        let _ = fs::remove_file(&path);
        return None;
    }

    Some(anchor.push_url())
}

/// Daemonize the current process via double-fork.
///
/// After this call returns `Ok(())`, the process is fully detached
/// from the terminal and running as a background daemon. The PID
/// file has been written.
///
/// Windows has no fork/setsid — background service semantics
/// there mean a service wrapper (sc.exe, NSSM) or a detached
/// relaunch, neither of which this in-process path can express.
/// Fail loudly rather than pretend.
#[cfg(not(unix))]
pub fn daemonize() -> Result<(), String> {
    Err("--daemon is not supported on this platform; \
         run `nmbrs web` in a separate terminal or under a \
         service wrapper instead"
        .into())
}

#[cfg(unix)]
pub fn daemonize() -> Result<(), String> {
    // First fork — parent exits, child continues.
    match unsafe { libc::fork() } {
        -1 => return Err("first fork failed".into()),
        0 => {}
        _ => std::process::exit(0),
    }

    // Create new session (detach from terminal).
    if unsafe { libc::setsid() } == -1 {
        return Err("setsid failed".into());
    }

    // Second fork — session leader exits, grandchild continues.
    match unsafe { libc::fork() } {
        -1 => return Err("second fork failed".into()),
        0 => {}
        _ => std::process::exit(0),
    }

    // Write PID file.
    let pid = std::process::id();
    let path = pid_file_path();
    fs::write(&path, pid.to_string())
        .map_err(|e| format!("failed to write PID file {}: {e}", path.display()))?;

    // Redirect stdin/stdout/stderr to /dev/null.
    unsafe {
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, libc::STDIN_FILENO);
            libc::dup2(devnull, libc::STDOUT_FILENO);
            libc::dup2(devnull, libc::STDERR_FILENO);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }

    Ok(())
}

/// Remove the anchor file if the recorded PID is no longer alive.
///
/// Called on startup so that a stale anchor from a crashed daemon
/// doesn't confuse port-in-use diagnostics.
pub fn cleanup_stale_anchor() {
    if let Some(anchor) = read_anchor() {
        let alive = pid_alive(anchor.pid);
        if !alive {
            eprintln!(
                "nmbrs web: cleaning up stale anchor (pid {} no longer running)",
                anchor.pid
            );
            remove_anchor();
        }
    }
}

/// Check whether the target address/port is available for binding.
///
/// Returns `Ok(())` if the port is free, or `Err(message)` with
/// actionable diagnostics (including the owning PID from the anchor
/// file, if available).
pub fn check_port_available(addr: &SocketAddr) -> Result<(), String> {
    match std::net::TcpListener::bind(addr) {
        Ok(_listener) => Ok(()), // drops immediately, freeing the port
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let mut msg = format!("port {} is already in use on {}", addr.port(), addr.ip());
            if let Some(anchor) = read_anchor() {
                let alive = pid_alive(anchor.pid);
                if alive {
                    msg.push_str(&format!(
                        "\n  → an nmbrs web instance is running (pid {})\n  → use 'nmbrs web --stop' to stop it, or 'nmbrs web --restart' to restart",
                        anchor.pid
                    ));
                } else {
                    msg.push_str("\n  → stale anchor file found (process dead) — another program may hold the port");
                    remove_anchor();
                }
            } else {
                // No anchor — check process table for nmbrs web processes.
                let procs = find_nmbrs_web_processes();
                if procs.is_empty() {
                    // Try to identify what's holding the port via ss/lsof
                    let port = addr.port();
                    let holder = identify_port_holder(port);
                    if let Some(info) = holder {
                        msg.push_str(&format!("\n  → held by: {info}"));
                    } else {
                        msg.push_str("\n  → another program is using this port");
                    }
                    msg.push_str("\n  → try a different port with port=<N>");
                } else {
                    for p in &procs {
                        msg.push_str(&format!(
                            "\n  → found nmbrs web process: pid {} — {}",
                            p.pid, p.cmdline
                        ));
                    }
                    msg.push_str("\n  → use 'nmbrs web --restart' to kill and restart");
                }
            }
            Err(msg)
        }
        Err(e) => Err(format!("cannot bind to {addr}: {e}")),
    }
}

/// Information about a running `nmbrs web` process found via /proc scan.
pub struct NmbrsWebProcess {
    pub pid: u32,
    pub cmdline: String,
}

/// Scan the process table for `nmbrs` processes whose command line
/// contains "web", excluding the current process.
///
/// Uses `/proc/*/cmdline` on Linux. Returns an empty vec on
/// non-Linux or if `/proc` is unavailable.
pub fn find_nmbrs_web_processes() -> Vec<NmbrsWebProcess> {
    let my_pid = std::process::id();
    let mut results = Vec::new();

    let Ok(entries) = fs::read_dir("/proc") else {
        return results;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }

        let cmdline_path = entry.path().join("cmdline");
        let Ok(raw) = fs::read(&cmdline_path) else {
            continue;
        };

        // /proc/*/cmdline uses NUL separators between args.
        let cmdline: String = raw
            .iter()
            .map(|&b| if b == 0 { ' ' } else { b as char })
            .collect::<String>()
            .trim()
            .to_string();

        // Match processes that look like "nmbrs web ..."
        if cmdline.contains("nmbrs") && cmdline.contains("web") {
            results.push(NmbrsWebProcess { pid, cmdline });
        }
    }

    results
}

/// Try to identify what process holds a given TCP port.
///
/// Uses `ss -tlnp` on Linux. Returns a human-readable description
/// like "pid 1234 (nginx)" or None if it can't determine.
fn identify_port_holder(port: u16) -> Option<String> {
    let output = std::process::Command::new("ss")
        .args(["-tlnp", &format!("sport = :{port}")])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse ss output: look for lines containing the port
    for line in stdout.lines().skip(1) {
        if line.contains(&format!(":{port}")) {
            // Extract the process info from the "users:" field
            // Format: users:(("program",pid=1234,fd=5))
            if let Some(users_start) = line.find("users:((") {
                let rest = &line[users_start + 8..];
                if let Some(end) = rest.find("))") {
                    return Some(rest[..end].to_string());
                }
            }
            // If no users field, return the whole line trimmed
            return Some(line.trim().to_string());
        }
    }
    None
}

/// Prompt the user on stderr/stdin with a yes/no question.
///
/// Returns `true` if the user answers `y` or `yes` (case-insensitive).
/// Returns `false` on `n`, `no`, EOF, or any other input.
pub fn confirm_prompt(message: &str) -> bool {
    eprint!("{message} [y/N] ");
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Terminate a process by PID and wait briefly for it to exit.
/// Unix: SIGTERM first, escalating to SIGKILL if it lingers.
/// Windows: hard TerminateProcess (see [`signal_terminate`]).
pub fn kill_pid(pid: u32) -> Result<(), String> {
    if !signal_terminate(pid) {
        return Err(format!("failed to signal pid {pid}"));
    }
    // Wait briefly for the process to exit.
    std::thread::sleep(std::time::Duration::from_millis(500));
    // Verify it actually exited.
    #[cfg(unix)]
    if pid_alive(pid) {
        eprintln!("nmbrs web: pid {pid} still alive after SIGTERM, sending SIGKILL");
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(())
}

/// Stop a running daemon by reading the PID file and sending SIGTERM.
pub fn stop_daemon() -> Result<(), String> {
    let path = pid_file_path();
    let pid_str = fs::read_to_string(&path).map_err(|_| {
        format!(
            "no running daemon found (no PID file at {})",
            path.display()
        )
    })?;
    let pid: i32 = pid_str
        .trim()
        .parse()
        .map_err(|_| "invalid PID file contents".to_string())?;

    // Verify the process exists and is an nmbrs process.
    let cmdline_path = format!("/proc/{pid}/cmdline");
    if let Ok(cmdline) = fs::read_to_string(&cmdline_path)
        && !cmdline.contains("nmbrs")
    {
        let _ = fs::remove_file(&path);
        return Err(format!(
            "PID {pid} is not an nmbrs process — stale PID file removed"
        ));
    }

    if !signal_terminate(pid as u32) {
        let _ = fs::remove_file(&path);
        return Err(format!(
            "failed to signal PID {pid} — stale PID file removed"
        ));
    }

    // Wait briefly for the process to exit.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = fs::remove_file(&path);
    // Also clean up the anchor file.
    remove_anchor();
    eprintln!("nmbrs web: stopped daemon (pid {pid})");
    Ok(())
}

// ── cli_spec entry ─────────────────────────────────────────

/// `nmbrs web` — daemon command. raw_args=true: the flag set
/// is mostly fine for the walker but the handler currently
/// expects argv[0] == "web" (it's invoked via `daemon::web_command(&args)`
/// not `&args[1..]`), so route raw to keep the contract.
pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{
        Arity, Category, Command, Flag, Handler, Level, ParsedCommand, ValueProvider,
    };
    fn handle(p: ParsedCommand) -> Result<(), String> {
        // web_command expects argv with `web` as argv[0].
        let mut argv: Vec<String> = vec!["web".into()];
        argv.extend(p.raw.iter().cloned());
        web_command(&argv);
        Ok(())
    }
    Command {
        name: "web",
        help: "Run the nmbrs web daemon.",
        category: Category::Server,
        level: Level::FullSurface,
        flags: vec![
            Flag {
                long: "--bind",
                short: None,
                aliases: &[],
                arity: Arity::Value,
                value: ValueProvider::Custom(crate::completion::bind_addr_provider),
                help: "Bind address (e.g. 127.0.0.1).",
                repeatable: false,
            },
            Flag {
                long: "--port",
                short: None,
                aliases: &[],
                arity: Arity::Value,
                value: ValueProvider::None,
                help: "Listen port.",
                repeatable: false,
            },
            Flag {
                long: "--daemon",
                short: None,
                aliases: &[],
                arity: Arity::Bool,
                value: ValueProvider::None,
                help: "Detach as a background daemon.",
                repeatable: false,
            },
            Flag {
                long: "--stop",
                short: None,
                aliases: &[],
                arity: Arity::Bool,
                value: ValueProvider::None,
                help: "Stop a running daemon.",
                repeatable: false,
            },
            Flag {
                long: "--restart",
                short: None,
                aliases: &[],
                arity: Arity::Bool,
                value: ValueProvider::None,
                help: "Restart a running daemon.",
                repeatable: false,
            },
        ],
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}
