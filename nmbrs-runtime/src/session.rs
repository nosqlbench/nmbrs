// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Session: the root context for a workload run.
//!
//! A session has a human-readable ID, a directory for all diagnostic
//! artifacts (metrics, logs, flamegraphs), and is the root of the
//! component tree for metrics labeling.
//!
//! Session ID format: `{scenario}_{YYYYMMDD_HHmmss}`
//! Session directory: `logs/{session_id}/`
//!
//! All files from a run live under the session directory:
//! - `metrics.db` — SQLite metrics
//! - `flamegraph.svg` — profiler output
//! - `session.log` — diagnostic log (future)

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use nmbrs_metrics::component::{Component, ComponentState, attach};
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::metrics_query::MetricsQuery;

/// SRD-77 — one invocation of `nmbrs <verb>` within a
/// [`Session`]. The session is the persistent container;
/// each execution is the unit of "what was attempted at
/// this point in time, with what workload version, and how
/// did it dispose?" Every per-phase / per-metric row carries
/// the owning execution's [`Self::exec_id`] as a dimensional
/// label so cross-execution queries can scope cleanly.
///
/// Until the SRD-77 `refine` verb lands the per-session
/// monotonic registry, every session has exactly one
/// execution with `exec_id = 1`. The shape exists now so the
/// SRD-76 storage layer (`phase_outcomes` / `phase_errors`)
/// and the component tree's root labels (`session`,
/// `exec_id`) honour the eventual SRD-77 contract from day
/// one — no later schema migration is required.
#[derive(Clone)]
pub struct Execution {
    /// Monotonic per-session sequence (`1, 2, 3, ...`).
    /// SRD-77's `refine` verb bumps it; SRD-88's concurrent
    /// harness allocates a distinct id per in-flight execution.
    pub exec_id: u64,
    /// Which verb launched this execution: `"run"` /
    /// `"resume"` / `"refine"`. Operator-visible via the SRD-77
    /// `executions` table and replay header.
    pub verb: &'static str,
    /// Wall-clock nanos-since-epoch at execution start.
    pub started_at_nanos: i64,
    /// The workload's bare stem (the `workload=` dimensional
    /// label and the `executions.workload` column for THIS
    /// execution). Per SRD-88 this is execution-tier identity,
    /// not session-tier: N executions sharing one session each
    /// carry their own.
    pub workload: String,
    /// Scenario name for this execution (metadata, not a
    /// dimensional label).
    pub scenario: String,
    /// This execution's component — a child of the session
    /// component (SRD-88 §2). Carries the `exec_id` + `workload`
    /// labels; phase/activity components attach under it so
    /// every metric inherits this execution's identity. The
    /// session component above it carries only `session=<id>`
    /// and is shared by every concurrent execution.
    pub component: Arc<RwLock<Component>>,
}

impl Execution {
    /// Start an execution under `session`: derive its
    /// [`Execution::component`] as a child of the session
    /// component, labelled with this execution's `exec_id` +
    /// `workload` stem (SRD-88 §2 — the session is the shared
    /// common root, each execution derives from it). `exec_id`
    /// is `1` for a fresh `run`/`resume`, the refine plan's
    /// `next_exec_id` for `refine`, or a distinct allocated id
    /// for a concurrent execution.
    pub fn start(
        session: &Session,
        workload: &str,
        scenario: &str,
        verb: &'static str,
        exec_id: u64,
    ) -> Self {
        let workload_stem = Path::new(workload)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("workload");
        // Execution-tier labels: `exec_id` + `workload`. The
        // parent session component already carries `session`;
        // `attach` composes the two so descendant metrics see
        // the full `{session, exec_id, workload}` set, exactly
        // as the pre-SRD-88 single-tier root did.
        let component = Arc::new(RwLock::new(Component::new(
            Labels::of("exec_id", exec_id.to_string()).with("workload", workload_stem),
            std::collections::HashMap::new(),
        )));
        attach(&session.component, &component);
        component
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .set_state(ComponentState::Running);
        Self {
            exec_id,
            verb,
            started_at_nanos: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
            workload: workload_stem.to_string(),
            scenario: scenario.to_string(),
            component,
        }
    }
}

impl std::fmt::Debug for Execution {
    /// Identity fields only — the component is an `Arc<RwLock<…>>`
    /// into the live tree and not meaningfully `Debug`-printable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Execution")
            .field("exec_id", &self.exec_id)
            .field("verb", &self.verb)
            .field("started_at_nanos", &self.started_at_nanos)
            .field("workload", &self.workload)
            .field("scenario", &self.scenario)
            .finish_non_exhaustive()
    }
}

/// A workload run session.
///
/// The session is the root of the component tree and — once the
/// runner has installed one — holds the shared [`MetricsQuery`] that
/// every in-process reader (TUI, summary, Polydat metric nodes) reads
/// through. See SRD-42 §"MetricsQuery — the unified read interface".
///
/// Per SRD-77, the session is a persistent container; the
/// [`Execution`] carried in [`Self::execution`] is the unit
/// of in-flight work. Both ride on the component tree as the
/// `session` + `exec_id` dimensional labels so every metric
/// the descendant components emit inherits them.
pub struct Session {
    /// Session identifier (the session-directory basename).
    /// Surfaces as the `session` dimensional label on every
    /// per-component metric via [`Self::component`].
    pub id: String,
    /// Output directory for diagnostic artifacts (metrics, logs, flamegraphs).
    /// Located at `logs/{id}/`. Not the working directory.
    pub output_dir: PathBuf,
    /// Session root component — **one per process** (SRD-88 §2).
    /// Owns the `session=<id>` dimensional label and nothing
    /// else; per-execution identity (`exec_id`, `workload`)
    /// lives on the [`Execution::component`] children that hang
    /// under it. Each label name is owned by exactly one tier
    /// and never redeclared below it.
    pub component: Arc<RwLock<Component>>,
    /// Shared `MetricsQuery` handle — installed by the runner once the
    /// cadence reporter is built. `None` before the runner wires it.
    pub metrics_query: Mutex<Option<Arc<MetricsQuery>>>,
}

/// Translate a CLI flag name (`--session-path`) into its
/// canonical `NMBRS_`-prefixed env-var name (`NMBRS_SESSION_PATH`).
/// Per SRD-04, every CLI flag automatically has an env-var
/// equivalent following this convention.
pub fn flag_env_name(flag: &str) -> String {
    let stem = flag.trim_start_matches("--");
    format!("NMBRS_{}", stem.replace('-', "_").to_ascii_uppercase())
}

/// Resolve a flag value from CLI args + its `NMBRS_`-prefixed
/// env var. Returns `None` if neither is set. **Exits with
/// status 2** if BOTH are set — configuration conflict; we
/// refuse to silently disambiguate.
///
/// Per SRD-04 the env-var name is automatically derived from
/// the flag name (`--foo-bar` → `NMBRS_FOO_BAR`).
pub fn resolve_flag(args: &[String], flag: &str) -> Option<String> {
    let cli = {
        let eq_prefix = format!("{flag}=");
        let mut iter = args.iter();
        let mut found = None;
        while let Some(arg) = iter.next() {
            if let Some(rest) = arg.strip_prefix(&eq_prefix) {
                found = Some(rest.to_string());
                break;
            }
            if arg == flag {
                found = iter.next().cloned();
                break;
            }
        }
        found
    };
    let env_name = flag_env_name(flag);
    let env = std::env::var(&env_name)
        .ok()
        .filter(|v| !v.trim().is_empty());
    match (cli, env) {
        (Some(_), Some(_)) => {
            eprintln!(
                "error: configuration conflict — both `{flag}` (CLI) and \
                 `{env_name}` (env) are set. Pick one. Per SRD-04, every \
                 CLI flag has an env equivalent prefixed with `NMBRS_`; \
                 specifying both at once is a hard error so the operator \
                 sees their inputs are fighting."
            );
            std::process::exit(2);
        }
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

/// Classify a session-path value: does it look like a
/// `key=value` workload-param token (e.g. `scenario=foo`)
/// rather than a real filesystem path? The umbrella
/// `--session <kv>` parser splits only on `:`, so a
/// `=`-shaped token slipping into the path slot would silently
/// materialise directories like `<cwd>/scenario=foo/...`.
///
/// Heuristic: the head of `head=tail` matches the workload-
/// param ABNF (alphanumeric / underscore / hyphen, no slash).
/// A real path like `/var/tmp/k=v` keeps a leading slash in
/// the head and passes through unchanged. Leading `./` or
/// `../` likewise excludes the head from the param-shape
/// check (the head contains a `/`).
///
/// Returns `Err(<error message>)` when the value should be
/// rejected; `Ok(())` when it's safe to use as a session
/// path.
pub fn check_session_path(p: &str, source: &str) -> Result<(), String> {
    if let Some((head, _)) = p.split_once('=') {
        let head_looks_like_param = !head.is_empty()
            && head
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            && !head.contains('/');
        if head_looks_like_param {
            return Err(format!(
                "session path from {source} is '{p}' — that looks \
                 like a `key=value` workload param, not a path. The umbrella \
                 `--session <kv>` parser splits only on `:`, so this would \
                 silently create a `<cwd>/{p}/…` directory tree. \
                 Did you mean: `--session-path <path>` (with `{p}` as a \
                 separate workload arg), or `--session path:<path>` (umbrella \
                 form, `:` as the kv separator)?"
            ));
        }
    }
    Ok(())
}

/// Wrapper around [`check_session_path`] that prints to
/// stderr and exits with code 2 on rejection. Used at every
/// entry point that sets `session_path` from CLI / env input:
/// [`parse_session_kv`] (bare-token branch and `path:`/`dir:`
/// keys), [`resolve_session_dir`] for `--session-path`, and
/// the legacy `SESSION_DIRECTORY` env fallback. Same exit-
/// shape as the existing `--session` configuration-conflict
/// error in [`resolve_flag`].
pub(crate) fn validate_session_path_or_exit(p: &str, source: &str) {
    if let Err(msg) = check_session_path(p, source) {
        eprintln!("error: {msg}");
        std::process::exit(2);
    }
}

/// Legacy env-var name for `--session-path`. Pre-SRD-04
/// shipping name that some operators may have in their shell
/// config; honored as a deprecated fallback below
/// `NMBRS_SESSION_PATH` and warns when read.
pub const SESSION_DIRECTORY_ENV: &str = "SESSION_DIRECTORY";

/// Token within a session-dir path that's replaced with the
/// auto-generated session id at write time. Lets users template
/// per-run directories without changing the path between runs:
/// `--session-dir /data/sessions/SESSION_run` →
/// `/data/sessions/default_20260101_120000_run`.
pub const SESSION_TOKEN: &str = "SESSION";

/// Policy when a session about to be created lands on an
/// existing, non-empty session directory.
///
/// Defaults to `Error` so accidental reuse can never silently
/// destroy a prior session's artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionReuse {
    /// Refuse to start. Exit with a clear message naming the
    /// existing path and the available reuse-mode flags.
    /// Default when `--session-reuse` is not specified.
    #[default]
    Error,
    /// Wipe the existing artifacts (`metrics.db`, `session.log`,
    /// `checkpoint.jsonl`, `summary.md`, etc.) and start fresh.
    /// The dir itself stays; only its contents are cleared.
    Restart,
    /// Continue with the existing session — equivalent to
    /// `--resume <session>` against this directory. Surfaces a
    /// hint if the operator probably wanted Restart.
    Resume,
}

impl SessionReuse {
    /// Parse from a string value (CLI flag or env var).
    /// Accepts `error` / `restart` / `resume`, case-insensitive.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" | "fail" | "abort" => Ok(Self::Error),
            "restart" | "wipe" | "clean" => Ok(Self::Restart),
            "resume" | "continue" => Ok(Self::Resume),
            other => Err(format!(
                "session-reuse: expected one of 'error' | 'restart' | 'resume', got '{other}'",
            )),
        }
    }
}

/// Where the default session directory lives when the user hasn't
/// passed `--session-path`. Three cases:
///
/// * **Normal invocation** (installed binary, user shell): the
///   session lands at `<cwd>/logs/<id>`. This is the documented,
///   long-standing user-facing behavior.
///
/// * **In-process tests that pre-sandboxed cwd** (e.g.
///   `checkpoint_resume_staircase` `in_dir(tmp, runner::run)`):
///   `<cwd>/logs/<id>` is already inside the test's tempdir
///   sandbox, so leave it alone — the test reads back from
///   `<cwd>/logs/latest` and would be broken by a redirect.
///
/// * **Cargo-spawned invocation where cwd lands inside the
///   workspace** (`cargo test` integration tests that spawn
///   `nmbrs` as a subprocess without `--session-path`, plus
///   `cargo run --bin nmbrs` runs from the workspace root):
///   the default would otherwise be the user-visible
///   `<workspace>/logs/<id>`, and the `session-keep` rotation
///   would evict real run sessions. Redirect into
///   `$TMPDIR/nmbrs-sessions/<id>` — `.cargo/config.toml`
///   already points `TMPDIR` at `<workspace>/target/test-tmp`,
///   so cargo-spawned sessions live in `target/test-tmp/`
///   alongside other test artifacts. See
///   `feedback_tests_no_project_root`.
///
/// Tests that want to read back their session contents must
/// still pass `--session-path` to a known path — this
/// fallback is the blast-radius limiter, not a substitute for
/// explicit sandboxing.
pub fn default_session_dir(id: &str) -> PathBuf {
    default_sessions_root().join(id)
}

/// Resolve the parent directory the runtime treats as the session
/// root when `--session-path` is absent. See [`default_session_dir`]
/// for the three-case logic.
/// `<root>/latest` — the symlink the runtime maintains to point
/// at the most recent session dir. Read-side commands
/// (`nmbrs report`, `nmbrs plot`, `nmbrs replay`, …) default to
/// reading through this path so "show me my latest run" is the
/// natural no-arg invocation.
pub fn latest_session_dir() -> PathBuf {
    default_sessions_root().join("latest")
}

/// `<root>/latest/metrics.db` — the sqlite db of the most recent
/// session. Centralised here so every read-side command names
/// the path one way.
pub fn latest_metrics_db() -> PathBuf {
    latest_session_dir().join("metrics.db")
}

/// `<root>/latest/session.log` — the diagnostic log of the most
/// recent session. `nmbrs attach` / log tooling reads from here.
pub fn latest_session_log() -> PathBuf {
    latest_session_dir().join("session.log")
}

/// `<root>/latest/checkpoint.jsonl` — the SRD-44 event log of
/// the most recent session. The resume planner reads from here.
pub fn latest_checkpoint_jsonl() -> PathBuf {
    latest_session_dir().join("checkpoint.jsonl")
}

/// `<root>/<name>` — a named session dir under the sessions root.
/// Used by `--session=<name>` resolvers in read-side commands so
/// the path doesn't get spelled out at every call site.
pub fn session_dir_named(name: &str) -> PathBuf {
    default_sessions_root().join(name)
}

/// Whether the CLI args request a dry-run (any `dryrun=<mode>` with a
/// non-empty, non-false value). A dry-run is resume-INERT: it must NOT
/// claim the `latest` symlink and must NOT write a checkpoint, so a
/// later `--resume-latest` can never pick a dry-run up and resume from
/// its (placeholder / short-circuited) phases (SRD-44). Detected from
/// raw args so both `Session::new_with_args` (the `latest` claim) and
/// `SessionHost::setup` (the checkpoint writer) gate on one signal.
pub fn args_request_dryrun(args: &[String]) -> bool {
    args.iter().any(|a| {
        let a = a.strip_prefix("--").unwrap_or(a);
        a.strip_prefix("dryrun=")
            .map(|v| {
                let v = v.trim();
                !v.is_empty() && v != "false" && v != "0"
            })
            .unwrap_or(false)
    })
}

/// Create a symlink at `link` pointing at `target` (which may be
/// relative to `link`'s parent dir). On Unix this is plain
/// `symlink`; Windows separates file and directory symlinks, so
/// resolve the target and pick the matching flavor. Windows
/// symlink creation needs Developer Mode (or admin) — callers
/// treat failure as best-effort, same as on Unix.
pub(crate) fn symlink_any(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        // Callers clear a previous link with `remove_file`, which
        // on Windows cannot delete a DIRECTORY symlink or junction
        // (those are directory entries) — clean up leftovers here
        // so re-pointing works.
        if let Ok(md) = std::fs::symlink_metadata(link)
            && (md.file_type().is_symlink() || md.file_type().is_dir())
        {
            let _ = std::fs::remove_file(link);
            let _ = std::fs::remove_dir(link);
        }
        let resolved = match link.parent() {
            Some(p) => p.join(target),
            None => target.to_path_buf(),
        };
        if resolved.is_dir() {
            // Real symlinks need Developer Mode / admin; a
            // directory junction needs no privilege and reads back
            // through `fs::read_link` just the same (with an
            // absolute target — consumers here already resolve
            // both shapes). Junction targets must be absolute.
            std::os::windows::fs::symlink_dir(target, link).or_else(|_| {
                let abs = std::path::absolute(&resolved)?;
                junction::create(&abs, link)
            })
        } else {
            // File fallback: a same-volume hard link. Appends to
            // the original show up through it (same file), and the
            // artifact links are recreated at each session
            // start / artifact production, which bounds staleness.
            std::os::windows::fs::symlink_file(target, link)
                .or_else(|_| std::fs::hard_link(&resolved, link))
        }
    }
}

/// Point `<sessions-root>/latest` at `session_dir`, but only when
/// the dir lives under the sessions root — a path the user
/// redirected elsewhere (`--session-path /tmp/x`) is left alone,
/// same guard as the startup hook. Best-effort: symlink failures
/// warn rather than abort.
pub fn point_latest_at(session_dir: &Path) {
    let root = default_sessions_root();
    if !target_is_under(&root, session_dir) {
        return;
    }
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    let latest = root.join("latest");
    let relative_target = relative_symlink_target(&latest, session_dir);
    let _ = std::fs::remove_file(&latest);
    let _ = symlink_any(&relative_target, &latest);
}

/// Initialize a NEW, empty session: create its directory, the
/// `metrics.db` schema, and the invariant `session` metadata — but
/// record NO execution (nothing has run). Points `latest` at it.
/// A later `run`/`refine` attaches the first execution.
///
/// `explicit_path` overrides the default `<sessions-root>/<id>`
/// location. `reuse` governs an already-populated directory.
pub fn init_empty_session(
    id: &str,
    explicit_path: Option<&Path>,
    reuse: SessionReuse,
) -> Result<PathBuf, String> {
    let dir = match explicit_path {
        Some(p) => p.to_path_buf(),
        None => default_session_dir(id),
    };
    let metrics_db = dir.join("metrics.db");
    if metrics_db.exists() {
        match reuse {
            SessionReuse::Error => {
                return Err(format!(
                    "session '{id}' already exists at {} — pass session-reuse=restart to \
                 overwrite, session-reuse=resume to keep it, or choose another name",
                    dir.display(),
                ));
            }
            SessionReuse::Restart => {
                let _ = std::fs::remove_file(&metrics_db);
            }
            SessionReuse::Resume => return Ok(dir),
        }
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create session dir {}: {e}", dir.display()))?;
    {
        // `SqliteReporter::new` creates the schema; writing the
        // invariant `session` key seeds session_metadata. Dropping
        // the reporter flushes (writes auto-commit).
        let mut reporter = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&metrics_db)
            .map_err(|e| format!("create metrics.db at {}: {e}", metrics_db.display()))?;
        reporter.set_metadata("session", id);
    }
    point_latest_at(&dir);
    Ok(dir)
}

pub fn default_sessions_root() -> PathBuf {
    if cwd_is_workspace_dir() {
        // Cargo-spawned invocation, cwd is a cargo workspace
        // dir (`Cargo.toml` present) — the cwd-relative default
        // would write into the user-visible `<workspace>/sessions/`.
        // Redirect to TMPDIR (per `.cargo/config.toml`, this is
        // `<workspace>/target/test-tmp/`), under an
        // `nmbrs-sessions/` infix so siblings tempfiles stay
        // segregated.
        std::env::temp_dir().join("nmbrs-sessions")
    } else {
        // SRD-77: `sessions/` is the per-cwd default. The
        // directory holds session state (metrics.db,
        // session.log, checkpoints, reports), not just logs.
        // Pre-SRD-77 builds wrote to `logs/`; the migration is
        // flag-day (no auto-rename — operators run a one-shot
        // `mv logs sessions` if they care about retention).
        PathBuf::from("sessions")
    }
}

/// `true` when both (a) we're running under cargo
/// (`CARGO_MANIFEST_DIR` env present, inherited from the cargo
/// parent through `std::process::Command`'s default env-inherit)
/// and (b) the current working directory has a `Cargo.toml` at
/// its root (workspace member or workspace root). Tests that
/// sandbox cwd into a tempdir won't satisfy (b) — the redirect
/// stays off for those.
fn cwd_is_workspace_dir() -> bool {
    if std::env::var_os("CARGO_MANIFEST_DIR").is_none() {
        return false;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join("Cargo.toml").is_file())
        .unwrap_or(false)
}

/// Resolved session inputs from CLI / env. Built by
/// [`resolve_session_dir`] from one of:
///
/// - The umbrella `--session <kv-list>` flag (a
///   comma-separated list of `key:value` pairs and bare
///   shortcuts).
/// - Per-key long-form flags (`--session-name`,
///   `--session-path`, `--session-reuse`, `--session-keep`,
///   `--session-shelflife`).
/// - `SESSION_DIRECTORY` env var (equivalent to
///   `--session-path`).
///
/// When both forms are present the long-form flag wins; the
/// umbrella flag is shorthand. Within the umbrella value the
/// last-wins rule applies if a key is repeated.
#[derive(Debug, Clone, Default)]
pub struct SessionDirSpec {
    /// `name` — session id (basename of the session directory).
    /// Used in metric labels and as the resolved value of the
    /// `SESSION` token in `path`. When unset, defaults to the
    /// auto-generated `<scenario>_<timestamp>` form.
    pub session_name: Option<String>,
    /// `path` — full directory path. Optional `SESSION` token
    /// inside the path is replaced with `session_name` at write
    /// time. When unset, the path is `logs/<session_name>/`.
    pub session_path: Option<String>,
    /// `reuse` — policy when the resolved directory already
    /// contains prior session artifacts. Default `Error`.
    pub reuse: SessionReuse,
    /// `keep` — number of session directories retained under
    /// the parent at startup. Default 10. `0` disables.
    pub session_keep: usize,
    /// `shelflife` — max age of a session directory before
    /// purge at startup. Default 4 weeks. `0` disables.
    pub session_shelflife: std::time::Duration,
    /// SRD-106 — the umbrella bare token `new` (`--session new`):
    /// force a fresh auto-named session. Its only consumer is the
    /// `stick_session` resolution rung, which it defeats; with no
    /// stick in play a fresh session is already the default, so
    /// the token is a harmless no-op there.
    pub force_new: bool,
}

impl SessionDirSpec {
    /// `true` when no path/name flag is set — caller falls back
    /// to the default `logs/<auto_id>/` behavior. (`reuse`,
    /// `keep`, `shelflife` are always present at their defaults
    /// and don't count toward "is this empty?".)
    pub fn is_empty(&self) -> bool {
        self.session_name.is_none() && self.session_path.is_none()
    }

    /// Resolve to a concrete `(output_dir, session_id)`.
    ///
    /// - `auto_id` is the fallback session name when
    ///   `session_name` is unset.
    /// - The resolved id = `session_name.unwrap_or(auto_id)`.
    /// - The resolved path = `session_path` (with `SESSION`
    ///   token replaced by the resolved id) if set, else
    ///   `logs/<id>/`.
    ///
    /// Returns `None` only when the spec is empty and the
    /// caller should use defaults; otherwise `Some` with the
    /// resolved values.
    pub fn resolve(&self, auto_id: &str) -> Option<(PathBuf, String)> {
        if self.is_empty() {
            return None;
        }
        let id = self
            .session_name
            .clone()
            .unwrap_or_else(|| auto_id.to_string());
        let path = match &self.session_path {
            Some(p) => PathBuf::from(p.replace(SESSION_TOKEN, &id)),
            None => default_session_dir(&id),
        };
        // Re-derive id from basename so a path-only spec
        // (`--session-path /tmp/foo`) still yields id="foo".
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
            .unwrap_or(id);
        Some((path, id))
    }

    /// `true` if the resolved path needs the auto-id (i.e.
    /// neither `session_name` nor a fully-concrete
    /// `session_path` is set). Used at startup to decide
    /// whether `logs/latest` can be wired immediately or must
    /// wait for session creation.
    pub fn needs_auto_id(&self) -> bool {
        if self.session_name.is_some() {
            return false;
        }
        match self.session_path.as_deref() {
            Some(p) => p.contains(SESSION_TOKEN),
            None => true,
        }
    }
}

/// Parse the umbrella `--session <kv-list>` argument into a
/// [`SessionDirSpec`]. The list is comma-separated; each item
/// is either a bare shortcut (`restart`, `resume`, `error`)
/// or a `key:value` pair.
///
/// Recognised keys (and their long-form flag equivalents):
///
/// | Key            | Long-form flag             |
/// | -------------- | -------------------------- |
/// | `name`         | `--session-name`           |
/// | `path` / `dir` | `--session-path`           |
/// | `reuse`        | `--session-reuse`          |
/// | `keep`         | `--session-keep`           |
/// | `shelflife`    | `--session-shelflife`      |
///
/// Bare shortcuts:
///
/// | Token     | Equivalent     |
/// | --------- | -------------- |
/// | `restart` | `reuse:restart`|
/// | `resume`  | `reuse:resume` |
/// | `error`   | `reuse:error`  |
/// | `new`     | force a fresh auto-named session (defeats `stick_session`) |
///
/// Whitespace around items + key/value separators is trimmed.
/// Unknown keys produce a `Warn` log; the rest of the spec is
/// kept (so a typo doesn't kill the run).
pub fn parse_session_kv(s: &str) -> SessionDirSpec {
    let mut spec = SessionDirSpec {
        session_keep: DEFAULT_SESSIONS_MAX,
        session_shelflife: DEFAULT_SESSIONS_SHELFLIFE,
        ..SessionDirSpec::default()
    };
    for raw_item in s.split(',') {
        let item = raw_item.trim();
        if item.is_empty() {
            continue;
        }
        // Bare shortcut?
        match item {
            "restart" => {
                spec.reuse = SessionReuse::Restart;
                continue;
            }
            "resume" => {
                spec.reuse = SessionReuse::Resume;
                continue;
            }
            "error" => {
                spec.reuse = SessionReuse::Error;
                continue;
            }
            // SRD-106 — `--session new`: force a fresh auto-named
            // session, defeating a workload's `stick_session`.
            // Intercepted here so the name heuristic below never
            // reads it as `name:new`.
            "new" => {
                spec.force_new = true;
                continue;
            }
            _ => {}
        }
        // A Windows drive-letter path (`C:\…` or `C:/…`) would
        // split at its drive colon and read as unknown key "C" —
        // recognize it as a bare path token before the key:value
        // parse gets a look.
        let drive_letter_path = item.len() >= 3
            && item.as_bytes()[0].is_ascii_alphabetic()
            && item.as_bytes()[1] == b':'
            && matches!(item.as_bytes()[2], b'\\' | b'/');
        // key:value pair takes precedence over the
        // bare-token heuristics so `dir:/tmp/x` etc.
        // disambiguate cleanly.
        let kv = if drive_letter_path {
            None
        } else {
            item.split_once(':')
        };
        let (key, value) = match kv {
            Some((k, v)) => (k.trim(), v.trim()),
            None => {
                // Bare token without `:`. Two heuristics:
                //   - contains a path separator OR resolves to an
                //     existing directory → treat as `path:<value>`.
                //     Lets operators write
                //     `--session logs/fulltest_2026...` without
                //     remembering the `path:` prefix.
                //   - otherwise → session name (SRD-04
                //     most-specific-name rule).
                if drive_letter_path
                    || item.contains('/')
                    || (cfg!(windows) && item.contains('\\'))
                    || std::path::Path::new(item).is_dir()
                {
                    validate_session_path_or_exit(item, "umbrella `--session <bare-token>`");
                    spec.session_path = Some(item.to_string());
                } else {
                    spec.session_name = Some(item.to_string());
                }
                continue;
            }
        };
        match key {
            "name" => spec.session_name = Some(value.to_string()),
            "path" | "dir" => {
                validate_session_path_or_exit(value, "umbrella `--session path:<v>`");
                spec.session_path = Some(value.to_string());
            }
            "reuse" => match SessionReuse::parse(value) {
                Ok(r) => spec.reuse = r,
                Err(e) => crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!("--session: {e}"),
                ),
            },
            "keep" => match value.parse::<usize>() {
                Ok(n) => spec.session_keep = n,
                Err(_) => crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!("--session: keep:{value:?} is not a non-negative integer"),
                ),
            },
            "shelflife" => match parse_duration(value) {
                Ok(d) => spec.session_shelflife = d,
                Err(e) => crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!("--session: shelflife: {e}"),
                ),
            },
            other => crate::observer::log(
                crate::observer::LogLevel::Warn,
                &format!(
                    "--session: unknown key {other:?} (recognised: name, path, dir, reuse, keep, shelflife)"
                ),
            ),
        }
    }
    spec
}

/// Parse the umbrella `--session <kv>` flag + per-key
/// long-form flags + env vars into a [`SessionDirSpec`].
///
/// **Umbrella form** — `--session <kv-list>` where `<kv-list>`
/// is comma-separated `key:value` pairs and bare shortcuts.
/// See [`parse_session_kv`] for the full key list.
///
/// **Long-form flags** — same keys, individually:
/// `--session-name`, `--session-path`, `--session-reuse`,
/// `--session-keep`, `--session-shelflife`. Both `=value` and
/// space-separated `<flag> <value>` shapes are accepted.
///
/// **Env vars:**
/// - `SESSION_DIRECTORY` → `--session-path`
/// - `SESSIONS_MAX` → `--session-keep`
/// - `SESSIONS_SHELFLIFE` → `--session-shelflife`
///
/// **Precedence** (highest first):
/// 1. Long-form per-key flag.
/// 2. Umbrella `--session` value (parsed left-to-right; later
///    keys override earlier ones).
/// 3. Env var equivalents.
/// 4. Default.
/// Resolve a flag written either canonically or under a documented alias.
///
/// Several spellings appeared only in the docs and were parsed nowhere, so an
/// invocation copied from the guide was read as an unknown flag and ignored — the
/// setting silently stayed at its default. The canonical spelling wins when both
/// appear; a differing alias value is reported rather than dropped, since two
/// disagreeing values on one command line means the operator expected something
/// this cannot deliver.
fn aliased_flag(args: &[String], canonical: &str, alias: &str) -> Option<String> {
    let canon = resolve_flag(args, canonical);
    let aliased = resolve_flag(args, alias);
    match (&canon, &aliased) {
        (Some(c), Some(a)) if c != a => crate::observer::log(
            crate::observer::LogLevel::Warn,
            &format!("{canonical}={c} and {alias}={a} disagree; using {canonical}={c}"),
        ),
        _ => {}
    }
    canon.or(aliased)
}

pub fn resolve_session_dir(args: &[String]) -> SessionDirSpec {
    // Each flag goes through `resolve_flag` which checks both
    // CLI and the auto-derived `NMBRS_<FLAG>` env var. Setting
    // both is a hard error.

    // Umbrella --session / NMBRS_SESSION first, so long-form
    // and per-key env can override individual fields.
    // Accept the bare `session=<spec>` spelling as well as `--session=<spec>`.
    // Read-side commands already take bare `workload=<file>`, so an operator
    // reasonably writes `session=<dir>` beside it — and before this, that form was
    // consumed as an unrecognised key=value and SILENTLY ignored, so the command
    // reported on `sessions/latest` while naming a different directory. A wrong
    // answer that looks right is the worst outcome available here.
    let bare_session = args
        .iter()
        .find_map(|a| a.strip_prefix("session="))
        .map(|v| v.to_string());
    let mut spec = match resolve_flag(args, "--session").or(bare_session) {
        Some(kv) => parse_session_kv(&kv),
        None => SessionDirSpec {
            session_keep: DEFAULT_SESSIONS_MAX,
            session_shelflife: DEFAULT_SESSIONS_SHELFLIFE,
            ..SessionDirSpec::default()
        },
    };

    // Long-form per-key flags (each with NMBRS_ env equivalent).
    if let Some(v) = resolve_flag(args, "--session-name") {
        spec.session_name = Some(v);
    }
    // `--session-dir` is an alias for `--session-path`. `Session::new_with_args`'s
    // own doc, and the user guide's flag table, both named `--session-dir` as the
    // way to set the directory — but nothing parsed it, so it was silently ignored
    // and the session landed at the default path. (`SESSION_DIRECTORY`, the legacy
    // env spelling the guide pairs with it, WAS honoured, which made the gap look
    // like an env-only feature.)
    if let Some(v) = aliased_flag(args, "--session-path", "--session-dir") {
        validate_session_path_or_exit(&v, "`--session-path` flag (or NMBRS_SESSION_PATH env)");
        spec.session_path = Some(v);
    }
    if let Some(v) = resolve_flag(args, "--session-reuse")
        && let Ok(r) = SessionReuse::parse(&v)
    {
        spec.reuse = r;
    }
    // Retention. `--session-keep` / `--session-shelflife` are canonical: they match
    // the rest of the `--session-*` family and the umbrella's `keep:` / `shelflife:`
    // sub-keys. The `--sessions-max` / `--sessions-shelflife` spellings are accepted
    // as aliases because the user guide documented ONLY those, so every invocation
    // written against it — `nmbrs run … --sessions-max=5` — was parsed as an unknown
    // flag and silently ignored, leaving retention at its default while the operator
    // believed they had changed it.
    if let Some(v) = aliased_flag(args, "--session-keep", "--sessions-max")
        && let Ok(n) = v.trim().parse::<usize>()
    {
        spec.session_keep = n;
    }
    if let Some(v) = aliased_flag(args, "--session-shelflife", "--sessions-shelflife")
        && let Ok(d) = parse_duration(&v)
    {
        spec.session_shelflife = d;
    }

    // Legacy env: SESSION_DIRECTORY is the pre-SRD-04 name for
    // NMBRS_SESSION_PATH. Honor it for back-compat with one
    // deprecation warning. Skip silently if NMBRS_SESSION_PATH
    // already won.
    if spec.session_path.is_none()
        && let Ok(v) = std::env::var(SESSION_DIRECTORY_ENV)
        && !v.trim().is_empty()
    {
        crate::observer::log(
            crate::observer::LogLevel::Warn,
            "SESSION_DIRECTORY is deprecated; use NMBRS_SESSION_PATH (SRD-04 NMBRS_-prefix convention).",
        );
        validate_session_path_or_exit(&v, "legacy `SESSION_DIRECTORY` env");
        spec.session_path = Some(v);
    }

    spec
}

/// Resolve the `--session` / `--session-path` / `--session-name`
/// arguments to a session directory path that `metrics.db`
/// would live under. Used by every read-side tool (`nmbrs plot`,
/// `nmbrs report`, `nmbrs metrics ...`, completion) so the same
/// flag means the same thing everywhere.
///
/// Returns `None` when no session flag is on the line — the
/// caller falls back to its own default (typically
/// `logs/latest`).
///
/// Never mutates the filesystem: no symlink rewrite, no purge. A pure path
/// computation, because read-side tools must not have side effects on the active
/// session symlink. This is now the ONLY way `--session` reaches a read command —
/// the startup hook used to additionally repoint `sessions/latest` at the named
/// session, which is exactly the side effect this doc warned against. See
/// [`purge_stale_sessions_at_startup`].
pub fn read_session_dir(args: &[String]) -> Option<PathBuf> {
    let spec = resolve_session_dir(args);
    if spec.is_empty() || spec.needs_auto_id() {
        return None;
    }
    spec.resolve("").map(|(p, _)| p)
}

/// Active-session resolver consolidating the patterns used by
/// `replay.rs` / `summary.rs` / `report_cmd.rs` /
/// `metricsql_cmd.rs` / `plot_metrics.rs` / `completion.rs`.
///
/// Resolves to an existing session directory in this order:
///
/// 1. `--session` / `--session-path` / `--session-name` from
///    `args` (via [`read_session_dir`]).
/// 2. The `logs/latest` symlink, when it exists and points at a
///    session directory with the expected artifacts
///    (`metrics.db` or `session.log`).
/// 3. `Err` with a remediation message naming the flags that
///    would have worked.
///
/// Read-only — no filesystem mutation. Use this anywhere a
/// post-run command needs to operate on an existing session.
///
/// **Why a separate function from [`read_session_dir`].**
/// `read_session_dir` returns `Option` (no opinion on what to do
/// when nothing's set); `resolve_active` makes that the call
/// site's failure mode, with a single canonical error message.
pub fn resolve_active(args: &[String]) -> Result<PathBuf, String> {
    if let Some(p) = read_session_dir(args) {
        if !p.exists() {
            return Err(format!(
                "session directory '{}' does not exist",
                p.display(),
            ));
        }
        return Ok(p);
    }
    let latest = latest_session_dir();
    if latest.exists() {
        // Resolve through the symlink so callers get a stable
        // path that won't change underneath them mid-run.
        let resolved = std::fs::canonicalize(&latest).unwrap_or(latest.clone());
        return Ok(resolved);
    }
    Err("no active session — run a workload first, or pass \
         `--session <name>` / `--session-path <dir>` to point \
         at an existing one"
        .to_string())
}

/// Default for `--session-keep` (alias `--sessions-max`): keep the
/// 10 most-recent sessions, purging older ones at the start of the
/// next session-creating command.
pub const DEFAULT_SESSIONS_MAX: usize = 10;

/// Default for `--session-shelflife` (alias `--sessions-shelflife`):
/// 4 weeks. Sessions older than this are purged regardless of the
/// `--session-keep` cap.
pub const DEFAULT_SESSIONS_SHELFLIFE: std::time::Duration =
    std::time::Duration::from_secs(60 * 60 * 24 * 7 * 4);

/// Parse a duration suffix-string. Accepts:
/// - `<n>s` — seconds
/// - `<n>m` — minutes
/// - `<n>h` — hours
/// - `<n>d` — days
/// - `<n>w` — weeks
/// - bare integer — seconds (back-compat with raw numeric input)
///
/// Whitespace is trimmed. `0` (any unit) disables the cap.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num_part, unit_seconds) = if let Some(n) = s.strip_suffix('w') {
        (n, 60 * 60 * 24 * 7)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 60 * 60 * 24)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 60 * 60)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1) // bare number = seconds
    };
    let n: u64 = num_part.trim().parse().map_err(|_| {
        format!("duration: '{s}' is not a valid number with optional s/m/h/d/w suffix",)
    })?;
    Ok(std::time::Duration::from_secs(n * unit_seconds))
}

/// Return `true` when `dir` exists AND contains artifacts that
/// indicate a prior session's state (metrics db, session log,
/// or checkpoint). Used by [`Session::new_with_args`] to decide
/// whether the reuse-policy check applies.
pub fn session_dir_has_prior_artifacts(dir: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    for marker in ["metrics.db", "session.log", "checkpoint.jsonl"] {
        if dir.join(marker).exists() {
            return true;
        }
    }
    false
}

/// Count directory entries under `parent` (excluding
/// symlinks, files, and the `latest` symlink target). Used
/// for end-of-run keep-cap forecasting.
pub fn count_session_dirs(parent: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            std::fs::symlink_metadata(e.path())
                .map(|m| !m.file_type().is_symlink() && m.file_type().is_dir())
                .unwrap_or(false)
        })
        .count()
}

/// Forecast how many session directories the **next** new
/// session would auto-purge under `parent` given the current
/// keep cap. Returns `0` when no purge would happen (or when
/// `keep_cap == 0`, which disables the cap).
///
/// Logged at INFO level by the end-of-run notice guard so
/// operators see the imminent cleanup before it happens, with
/// instructions for disabling it.
pub fn forecast_keep_purge(parent: &Path, keep_cap: usize) -> usize {
    if keep_cap == 0 {
        return 0;
    }
    let current = count_session_dirs(parent);
    // After +1 new session, total would be current+1. Anything
    // past keep_cap gets purged.
    (current + 1).saturating_sub(keep_cap)
}

/// `true` if `path` carries one of the signature artifacts an
/// nmbrs run writes — the gate the purge logic uses to avoid
/// destroying unrelated directories that happen to share a
/// parent with an explicit `--session-path`. Any one of
/// `metrics.db`, `session.log`, or `checkpoint.jsonl` is
/// enough; the runtime writes at least one of them per
/// session, so the test is robust across early-aborted runs.
fn looks_like_session_dir(path: &Path) -> bool {
    const SIGNATURES: &[&str] = &["metrics.db", "session.log", "checkpoint.jsonl"];
    SIGNATURES.iter().any(|s| path.join(s).exists())
}

/// Purge stale session directories under `parent` according to
/// `max_sessions` (keep the latest N) and `shelflife` (drop
/// anything older than this).
///
/// Skipped: the `latest` symlink and whatever it points at
/// (the active session). Non-directory entries (loose files
/// like a stray `summary.md`) are left alone.
///
/// Errors during enumeration / removal are logged at Warn
/// (this is a best-effort housekeeping pass; failure shouldn't
/// abort startup).
pub fn purge_stale_sessions(parent: &Path, max_sessions: usize, shelflife: std::time::Duration) {
    if !parent.exists() {
        return;
    }
    // Resolve the latest-symlink's target so we never delete
    // the active session out from under a running operator.
    let latest_target: Option<PathBuf> = std::fs::read_link(parent.join("latest"))
        .ok()
        .map(|t| if t.is_absolute() { t } else { parent.join(t) });

    // Collect (path, mtime) for every subdirectory.
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = match std::fs::read_dir(parent) {
        Ok(rd) => rd,
        Err(e) => {
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                &format!(
                    "warning: session cleanup: failed to read {}: {e}",
                    parent.display(),
                ),
            );
            return;
        }
    }
    .filter_map(|entry| entry.ok())
    .filter_map(|entry| {
        let path = entry.path();
        // Skip symlinks (logs/latest) — only cleanup real
        // directories.
        let md = std::fs::symlink_metadata(&path).ok()?;
        if md.file_type().is_symlink() || !md.file_type().is_dir() {
            return None;
        }
        // Don't delete the active session.
        if let Some(target) = latest_target.as_ref()
            && path == *target
        {
            return None;
        }
        // Only consider directories that *look like* nmbrs
        // sessions — i.e. carry one of the signature
        // artifacts the runtime writes. Without this, an
        // explicit `--session-path /tmp/foo` would set
        // `cleanup_parent = /tmp` and drag arbitrary
        // unrelated dirs (snap.rootfs_*, systemd-private-*,
        // …) into the purge set.
        if !looks_like_session_dir(&path) {
            return None;
        }
        let mtime = md.modified().ok()?;
        Some((path, mtime))
    })
    .collect();

    if entries.is_empty() {
        return;
    }

    // Sort newest-first for the max-cap pass.
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    let now = std::time::SystemTime::now();
    let mut to_purge: Vec<&PathBuf> = Vec::new();

    // Cap by count: anything past the max is purged.
    if max_sessions > 0 && entries.len() > max_sessions {
        for (p, _) in &entries[max_sessions..] {
            to_purge.push(p);
        }
    }

    // Cap by age: anything older than now - shelflife is purged.
    if !shelflife.is_zero() {
        for (p, mtime) in &entries[..entries.len().min(if max_sessions == 0 {
            usize::MAX
        } else {
            max_sessions
        })] {
            if let Ok(age) = now.duration_since(*mtime)
                && age > shelflife
                && !to_purge.contains(&p)
            {
                to_purge.push(p);
            }
        }
    }

    for path in to_purge {
        if let Err(e) = std::fs::remove_dir_all(path) {
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                &format!(
                    "warning: session cleanup: failed to remove {}: {e}",
                    path.display(),
                ),
            );
        }
    }
}

/// Run session-lifecycle cleanup at binary startup, honouring
/// `--session-keep` / `--session-shelflife`.
///
/// # What this used to also do, and why it no longer does
///
/// It used to REPOINT the `sessions/latest` symlink at whatever `--session`
/// named, on the theory that read-side commands defaulting to
/// `sessions/latest/metrics.db` would then target the right session for free.
/// That made `--session` work by mutating shared state: a read-only
/// `nmbrs table … --session=sessions/foo` left `latest` pointing at `foo`
/// afterwards, so a later bare `nmbrs report` — or a `--resume-latest` — silently
/// operated on `foo` instead of the newest real run. It also only worked for
/// sessions living under `sessions/`, so `--session=/tmp/x` behaved differently
/// from `--session=sessions/x` for no reason a caller could see.
///
/// Every command that legitimately OWNS `latest` claims it itself —
/// [`Session::new_with_args`] for a fresh run, [`Session::reattach`] for a
/// resume, [`init_empty_session`] for `session init` — so nothing was relying on
/// this to write the link. The read side now resolves `--session` locally
/// ([`read_session_dir`], and the per-command `resolve_db` helpers), which works
/// for any path and leaves `latest` alone.
///
/// A dry-run's deliberate refusal to claim `latest` is what made the cost of the
/// old behaviour concrete: see `args_request_dryrun` and SRD-44.
///
/// # Why this only runs for session-CREATING commands
///
/// The cleanup DELETES session directories (keeping the `--session-keep` most
/// recent, and dropping anything past `--session-shelflife`). It used to run on
/// every invocation, so a read-only `nmbrs table …` could destroy old sessions —
/// the destructive counterpart of the symlink rewrite described above. Retiring
/// old sessions belongs where sessions are ADDED, which is the only moment the
/// count grows; observing a session must never delete one.
///
/// `creating_session` comes from the caller, which knows the subcommand. Passing
/// `false` for an unrecognised command fails safe: cleanup is skipped, and the
/// next writing command performs it.
///
/// Failures log Warn and return; cleanup is best-effort.
pub fn purge_stale_sessions_at_startup(args: &[String], creating_session: bool) {
    if !creating_session {
        return;
    }
    let spec = resolve_session_dir(args);

    // Lifecycle cleanup runs unconditionally — it consults
    // `--session-keep` / `--session-shelflife` (with defaults).
    // Targets the parent dir: `--logs-dir` if specified, else
    // `logs/` under cwd. When `--session-dir` is explicit, its
    // *parent* directory is the cleanup target.
    let cleanup_parent = if let Some(sd) = spec.session_path.as_ref() {
        let resolved = sd.replace(SESSION_TOKEN, "");
        PathBuf::from(resolved)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(default_sessions_root)
    } else {
        default_sessions_root()
    };
    purge_stale_sessions(&cleanup_parent, spec.session_keep, spec.session_shelflife);
}

/// True when `target`'s absolute path is `logs_dir`'s absolute
/// path or lies below it. Both paths are resolved against the
/// current cwd if relative; canonicalize is avoided so this works
/// for not-yet-created targets.
pub(crate) fn target_is_under(logs_dir: &Path, target: &Path) -> bool {
    let cwd = std::env::current_dir().ok();
    let abs = |p: &Path| -> Option<PathBuf> {
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            cwd.as_ref().map(|c| c.join(p))
        }
    };
    match (abs(logs_dir), abs(target)) {
        (Some(l), Some(t)) => t.starts_with(&l),
        _ => false,
    }
}

/// Compute a target string for a symlink at `link_path` that
/// addresses `target` via a path relative to the link's parent
/// directory. Falls back to the absolute target when neither
/// path can be canonicalised (e.g. target doesn't exist yet,
/// which is normal for `logs/latest -> <id>` at session-create
/// time).
///
/// Examples:
///   `logs/latest`, `logs/foo_20260504/`        → `foo_20260504`
///   `logs/latest`, `target/test-tmp/sandbox/`  → `../target/test-tmp/sandbox`
///   `logs/latest`, `/tmp/explore/`             → `/tmp/explore`  (no common root)
pub(crate) fn relative_symlink_target(link_path: &Path, target: &Path) -> PathBuf {
    let link_parent = link_path.parent().unwrap_or_else(|| Path::new("."));
    // Resolve both sides to absolute paths to compute a relative
    // route. `canonicalize` would also follow symlinks; we want
    // logical absolutes so we use cwd-prefixing for the
    // not-yet-existing target case.
    let cwd = std::env::current_dir().ok();
    let abs = |p: &Path| -> Option<PathBuf> {
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            cwd.as_ref().map(|c| c.join(p))
        }
    };
    let (Some(link_abs), Some(tgt_abs)) = (abs(link_parent), abs(target)) else {
        return target.to_path_buf();
    };
    let link_comps: Vec<_> = link_abs.components().collect();
    let tgt_comps: Vec<_> = tgt_abs.components().collect();
    let common = link_comps
        .iter()
        .zip(tgt_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        // Different roots (e.g. `/home/...` vs `/tmp/...`) —
        // can't express as a relative path without more `..`s
        // than is sensible. Fall back to absolute.
        return tgt_abs;
    }
    let ups = link_comps.len() - common;
    let mut rel = PathBuf::new();
    for _ in 0..ups {
        rel.push("..");
    }
    for c in &tgt_comps[common..] {
        rel.push(c.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    rel
}

impl Session {
    /// Create a new session. Picks the output directory in this
    /// priority order:
    ///
    /// 1. `--session-dir <path>` CLI flag (or
    ///    `SESSION_DIRECTORY` env var, equivalent). `SESSION`
    ///    token in the path is replaced with the auto-generated
    ///    `{session_name}_{timestamp}` name. The basename becomes
    ///    the session id.
    /// 2. `--logs-dir <parent>` and/or `--session <name>` CLI
    ///    flags. Compose into `<parent>/<name>` (defaulting to
    ///    `logs/<auto-id>` for any unspecified component).
    /// 3. Default — `logs/{session_name}_{timestamp}/`.
    ///
    /// `session_name` is the auto-id stem (the single-run caller
    /// passes the scenario name; a concurrent SRD-88 host passes
    /// a session-level name). `args` is the raw CLI args slice —
    /// pass an empty slice to get env-only resolution. The
    /// session carries only `session=<id>` identity; per-execution
    /// workload/scenario live on the [`Execution`] tier.
    pub fn new_with_args(session_name: &str, args: &[String]) -> Self {
        let timestamp = format_timestamp();
        let auto_id = format!("{session_name}_{timestamp}");

        let spec = resolve_session_dir(args);
        let (output_dir, id) = spec
            .resolve(&auto_id)
            .unwrap_or_else(|| (default_session_dir(&auto_id), auto_id.clone()));

        // Reuse-policy check. Only fires when the resolved
        // directory already holds prior session artifacts. The
        // resume path enters via `Session::resume`, not here, so
        // any pre-existing artifacts at this entry point are a
        // collision the operator must explicitly resolve.
        if session_dir_has_prior_artifacts(&output_dir) {
            match spec.reuse {
                SessionReuse::Error => {
                    eprintln!(
                        "error: session directory {} already contains artifacts \
                         (metrics.db / session.log / checkpoint.jsonl).\n  \
                         Pick a reuse policy:\n    \
                         --session-reuse=restart  (wipe artifacts, fresh run)\n    \
                         --session-reuse=resume   (continue with the prior session — \
                         equivalent to --resume <id>)\n  \
                         Or pick a different path via --session, --logs-dir, \
                         --session-dir, or SESSION_DIRECTORY env.",
                        output_dir.display(),
                    );
                    std::process::exit(2);
                }
                SessionReuse::Restart => {
                    for marker in [
                        "metrics.db",
                        "session.log",
                        "checkpoint.jsonl",
                        "checkpoint.lock",
                        "summary.md",
                        "summary.txt",
                        "summary.json",
                        "tui.dump",
                        "flamegraph.svg",
                        "flamegraph-perf.svg",
                        "flamegraph-perf.md",
                    ] {
                        let _ = std::fs::remove_file(output_dir.join(marker));
                    }
                    crate::observer::log(
                        crate::observer::LogLevel::Warn,
                        &format!(
                            "session-reuse=restart: wiped prior artifacts in {}",
                            output_dir.display(),
                        ),
                    );
                }
                SessionReuse::Resume => {
                    eprintln!(
                        "error: --session-reuse=resume requires --resume to actually \
                         continue the prior session. Add --resume (or --resume-latest) \
                         to your command line. Path: {}",
                        output_dir.display(),
                    );
                    std::process::exit(2);
                }
            }
        }

        // Create the output directory (and any missing parents)
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                &format!(
                    "warning: failed to create session output directory {}: {e}",
                    output_dir.display()
                ),
            );
        }

        // Refresh `logs/latest` → this session, then *clear* every
        // per-artifact convenience symlink from the previous
        // session. Optional artifacts (flamegraphs, summary,
        // tui.dump) get their convenience link only when their
        // writer actually produces the file — eager pre-creation
        // would leave a dangling link any time the run skips that
        // artifact (e.g. no `profiler=` on the CLI). The two
        // guaranteed-written artifacts (`session.log` / `metrics.db`)
        // are linked here so live tooling can `tail -f
        // logs/session.log` or open `logs/metrics.db` without
        // chasing the timestamped session id.
        let logs = default_sessions_root();
        // Only touch `logs/` when the session output dir is under
        // it. An explicit `--session-path /tmp/x` (or any redirect
        // outside `logs/`) is treated as opt-out:
        // - The mkdir below would otherwise stamp a stray
        //   `<cwd>/logs/` directory in test sandboxes / CI
        //   working trees that don't want it (the
        //   `feedback_tests_no_project_root` rule), and
        // - `logs/latest` shouldn't get hijacked by test fixtures
        //   or one-off `--session-path` runs.
        // A dry-run is resume-inert: it must not hijack `latest` (nor
        // the per-artifact convenience links). Leaving `latest` pointing
        // at the prior REAL run is exactly what lets a subsequent
        // `--resume-latest` resume that run instead of the throwaway
        // dry-run. See `args_request_dryrun` / SRD-44.
        if target_is_under(&logs, &output_dir) && !args_request_dryrun(args) {
            let _ = std::fs::create_dir_all(&logs);
            let latest = logs.join("latest");
            let _ = std::fs::remove_file(&latest);
            let _ = symlink_any(&latest_symlink_target(&output_dir, &logs, &id), &latest);
            for stale in [
                "summary.md",
                "flamegraph.svg",
                "flamegraph-perf.svg",
                "flamegraph-perf.md",
                "tui.dump",
            ] {
                let _ = std::fs::remove_file(logs.join(stale));
            }
            for artifact in ["session.log", "metrics.db"] {
                let link = logs.join(artifact);
                let _ = std::fs::remove_file(&link);
                // Relative target routes through the `latest`
                // symlink so swapping sessions updates every artifact
                // link in a single `latest` update.
                let target = PathBuf::from("latest").join(artifact);
                let _ = symlink_any(&target, &link);
            }
        }

        // The session component owns exactly one dimensional
        // label: `session=<id>`. Per SRD-88 §2 this is the shared
        // common root — one per process — and per the label-
        // ownership invariant it never carries `exec_id` /
        // `workload`; those are declared once, on each
        // [`Execution::component`] child, and composed in by the
        // component tree. Descendant metrics still see the full
        // `{session, exec_id, workload}` set, but every name is
        // owned by exactly one tier.
        let component =
            Component::root(Labels::of("session", &id), std::collections::HashMap::new());
        // Install the session root as the resolver backing for
        // Polydat runtime-context nodes (`control(...)`, `rate()`,
        // `concurrency()`, etc.). See SRD 12 §"Runtime context
        // nodes" and polydat/src/nodes/runtime_context.rs.
        crate::polydat_nodes::runtime_context::set_session_root(component.clone());

        Self {
            id,
            output_dir,
            component,
            metrics_query: Mutex::new(None),
        }
    }

    /// Backward-compat shim for callers that don't have CLI
    /// args handy. Equivalent to
    /// `Session::new_with_args(session_name, &[])` — resolves
    /// session-dir from `SESSION_DIRECTORY` env only.
    pub fn new(session_name: &str) -> Self {
        Self::new_with_args(session_name, &[])
    }

    /// Re-attach to a prior session — reuse its directory, id,
    /// and (consequently) its `metrics.db` so the attaching
    /// invocation appends to the same metrics history rather
    /// than starting fresh in a new dir. Per SRD-44 §"Wholesale
    /// metrics-purge", phases that re-run need their prior sample
    /// rows purged in-place; that requires writing to the same
    /// db, which requires reusing the same session dir. The id is
    /// read from the directory's basename, so it preserves the
    /// original timestamp suffix.
    ///
    /// Backs both `nmbrs resume` and `nmbrs refine`: the two differ
    /// only in the [`Execution`] the caller starts under this
    /// session (its `verb` and `exec_id`), which is now an
    /// execution-tier concern (SRD-88) — not a session-tier one.
    /// `session_name` is the fallback id stem used only when the
    /// directory basename can't be read.
    pub fn reattach(prior_session_dir: PathBuf, session_name: &str) -> Self {
        let id = prior_session_dir
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
            .unwrap_or_else(|| {
                // Fallback: synthesize a fresh id from the
                // session name + timestamp. Shouldn't fire in
                // practice (we resolved the dir to a real session
                // before calling here).
                format!("{session_name}_{}", format_timestamp())
            });

        // Re-establish the convenience symlinks under `logs/` so
        // `tail -f logs/session.log` and `sqlite3 logs/metrics.db`
        // resolve to this session's artifacts. Same shape as
        // Session::new — a no-op when the symlinks already point
        // here from a prior run. Skipped when the session lives
        // outside `logs/` (see Session::new for rationale: one-off
        // external session dirs shouldn't hijack `logs/latest`).
        let logs = default_sessions_root();
        if target_is_under(&logs, &prior_session_dir) {
            let latest = logs.join("latest");
            let _ = std::fs::remove_file(&latest);
            let _ = symlink_any(Path::new(&id), &latest);
            for artifact in ["session.log", "metrics.db"] {
                let link = logs.join(artifact);
                let _ = std::fs::remove_file(&link);
                let target = PathBuf::from("latest").join(artifact);
                let _ = symlink_any(&target, &link);
            }
        }

        // Session component carries `session=<id>` only — the
        // re-attached execution(s) declare `exec_id` / `workload`
        // on their own [`Execution::component`] children, exactly
        // as a fresh session does (see `new_with_args`).
        let component =
            Component::root(Labels::of("session", &id), std::collections::HashMap::new());
        crate::polydat_nodes::runtime_context::set_session_root(component.clone());

        Self {
            id,
            output_dir: prior_session_dir,
            component,
            metrics_query: Mutex::new(None),
        }
    }

    /// Create a `logs/<artifact>` convenience symlink that points
    /// (through `logs/latest`) at the named artifact in the
    /// current session's output dir. Idempotent — replaces any
    /// existing link with the same name. Call from the writer
    /// at the moment the artifact has actually been produced, so
    /// the convenience link never dangles.
    pub fn link_artifact(name: &str) {
        let logs = default_sessions_root();
        let link = logs.join(name);
        let _ = std::fs::remove_file(&link);
        let target = PathBuf::from("latest").join(name);
        let _ = symlink_any(&target, &link);
    }

    /// Install the shared [`MetricsQuery`] handle. Called once by the
    /// runner after it has planned the cadence tree and built the
    /// cadence reporter. Panics if called twice.
    pub fn set_metrics_query(&self, query: Arc<MetricsQuery>) {
        let mut slot = self.metrics_query.lock().unwrap_or_else(|e| e.into_inner());
        assert!(slot.is_none(), "session metrics_query already installed");
        *slot = Some(query);
    }

    /// Borrow the installed [`MetricsQuery`]. Returns `None` before
    /// the runner wires it.
    pub fn metrics_query(&self) -> Option<Arc<MetricsQuery>> {
        self.metrics_query
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Path to the SQLite metrics file for this session.
    pub fn metrics_path(&self) -> PathBuf {
        self.output_dir.join("metrics.db")
    }

    /// Path for a profiler output file.
    pub fn profiler_path(&self, suffix: &str) -> PathBuf {
        self.output_dir.join(format!("flamegraph{suffix}.svg"))
    }

    /// Path for an arbitrary session artifact.
    pub fn artifact_path(&self, filename: &str) -> PathBuf {
        self.output_dir.join(filename)
    }
}

/// Format the current time as `YYYYMMDD_HHmmss`.
/// Compute the symlink target string for `logs/latest`,
/// always relative. For sessions under `logs/<id>/` the link
/// resolves to a bare `{id}`; sessions outside `logs/` get an
/// `../...` route up to a common ancestor. Relative targets
/// keep the link portable across directory moves and readable
/// in `ls -la` output.
fn latest_symlink_target(output_dir: &Path, logs: &Path, id: &str) -> PathBuf {
    if output_dir.parent() == Some(logs) {
        return PathBuf::from(id);
    }
    let latest = logs.join("latest");
    relative_symlink_target(&latest, output_dir)
}

/// UTC datetime components for session-name templating, derived
/// from the same manual Gregorian math as [`format_timestamp`] (no
/// chrono dependency). Returns `(year, month, day, hour, minute,
/// second, epoch_millis, epoch_seconds)`.
pub fn utc_datetime_fields() -> (u64, u64, u64, u64, u64, u64, u128, u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.as_millis();
    let day_count = secs / 86400;
    let t = secs % 86400;
    let (year, month, day) = days_to_ymd(day_count);
    (
        year,
        month,
        day,
        t / 3600,
        (t % 3600) / 60,
        t % 60,
        millis,
        secs,
    )
}

fn format_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Convert epoch seconds to date/time components.
    // Simple implementation without chrono dependency.
    let days = secs / 86400;
    let time = secs % 86400;
    let hours = time / 3600;
    let minutes = (time % 3600) / 60;
    let seconds = time % 60;

    // Days since epoch to Y/M/D (simplified Gregorian)
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}{month:02}{day:02}_{hours:02}{minutes:02}{seconds:02}")
}

/// Current wall-clock time as `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
/// Used by the session log writer for human-readable line timestamps.
pub fn now_log_timestamp() -> String {
    format_log_timestamp(std::time::SystemTime::now())
}

/// Format a specific `SystemTime` in the same shape as
/// [`now_log_timestamp`]. Used by the failure-dump path
/// in `nmbrs-tui::observer` to render per-LogEntry
/// timestamps captured at log-emit time.
pub fn format_log_timestamp(t: std::time::SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let days = secs / 86400;
    let time = secs % 86400;
    let hours = time / 3600;
    let minutes = (time % 3600) / 60;
    let seconds = time % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Convert days since Unix epoch to (year, month, day).
/// Format Unix seconds as `MM-DD HH:MM:SS` (UTC).
///
/// Exposed because report tables now show WHEN a series started and last moved,
/// and the epoch value on its own is unreadable. Shares `days_to_ymd` with the
/// session-id formatter rather than restating the calendar arithmetic; the year
/// is omitted because these columns compare moments inside one run, where the
/// month and day are already the widest useful distinction.
pub fn format_utc_short(epoch_seconds: f64) -> String {
    if !epoch_seconds.is_finite() || epoch_seconds <= 0.0 {
        return "-".to_string();
    }
    let secs = epoch_seconds as u64;
    let (_, month, day) = days_to_ymd(secs / 86400);
    let t = secs % 86400;
    format!(
        "{month:02}-{day:02} {:02}:{:02}:{:02}",
        t / 3600,
        (t % 3600) / 60,
        t % 60
    )
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's date library
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Resolve an operator-supplied relative path INSIDE `base`, refusing
/// anything that could name a location elsewhere: an absolute path, a
/// `..` component, an empty path, or a Windows drive/prefix. Output
/// paths that a workload file can set (`metrics-log`, `trace_log`)
/// route through here, so a shared workload can only ever write into
/// the session's own directory tree.
pub fn confine_to_dir(base: &Path, rel: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let rel_path = Path::new(rel.trim());
    if rel.trim().is_empty() {
        return Err("empty path".to_string());
    }
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "'{rel}' escapes the session directory (`..` is not allowed)"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "'{rel}' is absolute; only paths relative to the session directory are accepted here"
                ));
            }
        }
    }
    Ok(base.join(rel_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format() {
        let ts = format_timestamp();
        // Should be 15 chars: YYYYMMDD_HHmmss
        assert_eq!(ts.len(), 15, "timestamp: {ts}");
        assert!(
            ts.contains('_'),
            "timestamp should contain underscore: {ts}"
        );
    }

    /// Serialize all tests that mutate process-global state
    /// (SESSION_DIRECTORY env var, cwd-relative `logs/` writes,
    /// etc.). Cargo runs tests in parallel by default; tests
    /// touching shared state must hold this lock for the
    /// duration of the operation.
    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn dryrun_detection_from_args() {
        let yes =
            |a: &[&str]| args_request_dryrun(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(yes(&["run", "workload=w", "dryrun=op"]));
        assert!(yes(&["run", "dryrun=phase,wiring"]));
        assert!(yes(&["run", "--dryrun=structure"]));
        // A real run — no dryrun signal.
        assert!(!yes(&["run", "workload=w", "scenario=incremental"]));
        // Explicit falsey values are not a dry-run.
        assert!(!yes(&["run", "dryrun=false"]));
        assert!(!yes(&["run", "dryrun=0"]));
        assert!(!yes(&["run", "dryrun="]));
    }

    #[test]
    fn session_id_format() {
        // Session construction installs the process-global session root as a
        // side effect (see `new_with_args`), racing every control-node test
        // that reads through it. Hold the same guard those tests hold.
        let _root_guard = crate::polydat_nodes::runtime_context::session_root_test_guard();
        let _g = env_test_lock();
        unsafe {
            std::env::remove_var(SESSION_DIRECTORY_ENV);
        }
        let session = Session::new("fknn_rampup");
        assert!(session.id.starts_with("fknn_rampup_"), "id: {}", session.id);
        let expected_root = default_sessions_root();
        assert!(
            session.output_dir.starts_with(&expected_root),
            "output_dir {} should start with {}",
            session.output_dir.display(),
            expected_root.display()
        );
    }

    #[test]
    fn session_paths() {
        // Session construction installs the process-global session root as a
        // side effect (see `new_with_args`), racing every control-node test
        // that reads through it. Hold the same guard those tests hold.
        let _root_guard = crate::polydat_nodes::runtime_context::session_root_test_guard();
        let _g = env_test_lock();
        unsafe {
            std::env::remove_var(SESSION_DIRECTORY_ENV);
        }
        let session = Session::new("smoke");
        assert!(session.metrics_path().ends_with("metrics.db"));
        assert!(session.profiler_path("").ends_with("flamegraph.svg"));
        assert!(
            session
                .profiler_path("-perf")
                .ends_with("flamegraph-perf.svg")
        );
    }

    /// Clear every env var that `resolve_session_dir` reads, so a
    /// `spec_*` test sees CLI-only inputs. Without this, a
    /// concurrent test that sets `NMBRS_SESSION_NAME` (or any
    /// peer var) collides with this test's CLI flag, hits the
    /// "configuration conflict" path in `resolve_flag`, and
    /// calls `process::exit(2)` — killing the entire test
    /// process and surfacing as an unrelated test failure.
    /// Must be called under [`env_test_lock`] so the cleanup
    /// holds for the duration of the spec resolution.
    fn clear_session_env() {
        // SAFETY: under env_test_lock, no other test thread is
        // touching these vars.
        unsafe {
            std::env::remove_var("NMBRS_SESSION");
            std::env::remove_var("NMBRS_SESSION_NAME");
            std::env::remove_var("NMBRS_SESSION_PATH");
            std::env::remove_var("NMBRS_SESSION_REUSE");
            std::env::remove_var("NMBRS_SESSION_KEEP");
            std::env::remove_var("NMBRS_SESSION_SHELFLIFE");
            std::env::remove_var(SESSION_DIRECTORY_ENV);
        }
    }

    #[test]
    fn spec_session_path_flag_yields_basename_id() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session-path=/tmp/explicit".into()];
        let spec = resolve_session_dir(&args);
        let (path, id) = spec.resolve("auto-id").unwrap();
        assert_eq!(path.to_str(), Some("/tmp/explicit"));
        assert_eq!(id, "explicit");
    }

    /// Flags that the docs named but nothing parsed.
    ///
    /// The user guide's quick reference listed `--sessions-max`,
    /// `--sessions-shelflife` and `--session-dir`; none were parsed, so an
    /// invocation copied from the guide was read as an unknown flag and IGNORED —
    /// retention silently stayed at 10 sessions / 4 weeks, and the session landed
    /// at the default path. They are aliases now, with the `--session-*` family
    /// spellings canonical.
    #[test]
    fn documented_flag_aliases_are_honoured() {
        let _g = env_test_lock();
        clear_session_env();

        let spec = resolve_session_dir(&["--sessions-max=5".to_string()]);
        assert_eq!(
            spec.session_keep, 5,
            "`--sessions-max` must set the keep cap"
        );

        let spec = resolve_session_dir(&["--sessions-shelflife=2w".to_string()]);
        assert_eq!(
            spec.session_shelflife,
            std::time::Duration::from_secs(14 * 24 * 3600),
            "`--sessions-shelflife` must set the retention window"
        );

        let spec = resolve_session_dir(&["--session-dir=/tmp/explicit".to_string()]);
        let (path, id) = spec.resolve("auto").unwrap();
        assert_eq!(
            path.to_str(),
            Some("/tmp/explicit"),
            "`--session-dir` must set the session path"
        );
        assert_eq!(id, "explicit");

        // The canonical spellings keep working, and win when both are given.
        let spec = resolve_session_dir(&["--session-keep=7".to_string()]);
        assert_eq!(spec.session_keep, 7);
        let spec = resolve_session_dir(&[
            "--session-keep=7".to_string(),
            "--sessions-max=99".to_string(),
        ]);
        assert_eq!(
            spec.session_keep, 7,
            "the canonical spelling must win over the alias"
        );
    }

    #[test]
    fn bare_session_kv_resolves_like_the_dash_flag() {
        // Read-side commands take bare `workload=<file>`, so an operator writes
        // `session=<dir>` beside it. Before this was accepted, that token was
        // consumed as an unrecognised key=value and IGNORED — the command
        // reported on `sessions/latest` while naming a different directory.
        let _g = env_test_lock();
        clear_session_env();
        let bare = resolve_session_dir(&["session=/tmp/explicit".to_string()])
            .resolve("auto-id")
            .unwrap();
        let dashed = resolve_session_dir(&["--session=/tmp/explicit".to_string()])
            .resolve("auto-id")
            .unwrap();
        assert_eq!(bare, dashed, "both spellings must name the same session");
        assert_eq!(bare.0.to_str(), Some("/tmp/explicit"));
    }

    #[test]
    fn dash_session_wins_over_bare_session() {
        // Explicit flag beats the bare param spelling, so a wrapper script that
        // appends `--session=` can override a workload-supplied `session=`.
        let _g = env_test_lock();
        clear_session_env();
        let args = vec![
            "session=/tmp/from-param".to_string(),
            "--session=/tmp/from-flag".to_string(),
        ];
        let (path, _) = resolve_session_dir(&args).resolve("auto-id").unwrap();
        assert_eq!(path.to_str(), Some("/tmp/from-flag"));
    }

    #[test]
    fn spec_session_name_only_yields_default_logs_dir() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session-name=alpha".into()];
        let (path, id) = resolve_session_dir(&args).resolve("autogen").unwrap();
        assert_eq!(path, default_sessions_root().join("alpha"));
        assert_eq!(id, "alpha");
    }

    #[test]
    fn spec_session_path_token_replaced_with_name() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec![
            "--session-path=/data/SESSION_run".into(),
            "--session-name=alpha".into(),
        ];
        let (path, id) = resolve_session_dir(&args).resolve("autogen").unwrap();
        assert_eq!(path.to_str(), Some("/data/alpha_run"));
        assert_eq!(id, "alpha_run");
    }

    #[test]
    fn spec_session_path_token_falls_back_to_auto_id_when_no_name() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session-path=/data/SESSION_run".into()];
        let (path, id) = resolve_session_dir(&args).resolve("autogen").unwrap();
        assert_eq!(path.to_str(), Some("/data/autogen_run"));
        assert_eq!(id, "autogen_run");
    }

    #[test]
    fn spec_space_form_session_path_flag() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session-path".into(), "/data/path".into()];
        let (path, _) = resolve_session_dir(&args).resolve("auto").unwrap();
        assert_eq!(path.to_str(), Some("/data/path"));
    }

    #[test]
    fn spec_falls_back_to_env() {
        let _g = env_test_lock();
        let prior = std::env::var(SESSION_DIRECTORY_ENV).ok();
        unsafe {
            std::env::set_var(SESSION_DIRECTORY_ENV, "/from/env");
        }
        let spec = resolve_session_dir(&[]);
        match prior {
            Some(v) => unsafe {
                std::env::set_var(SESSION_DIRECTORY_ENV, v);
            },
            None => unsafe {
                std::env::remove_var(SESSION_DIRECTORY_ENV);
            },
        }
        let (path, _) = spec.resolve("auto").unwrap();
        assert_eq!(path.to_str(), Some("/from/env"));
    }

    #[test]
    fn spec_cli_flag_overrides_env() {
        let _g = env_test_lock();
        let prior = std::env::var(SESSION_DIRECTORY_ENV).ok();
        unsafe {
            std::env::set_var(SESSION_DIRECTORY_ENV, "/from/env");
        }
        let args = vec!["--session-path=/from/cli".into()];
        let spec = resolve_session_dir(&args);
        match prior {
            Some(v) => unsafe {
                std::env::set_var(SESSION_DIRECTORY_ENV, v);
            },
            None => unsafe {
                std::env::remove_var(SESSION_DIRECTORY_ENV);
            },
        }
        let (path, _) = spec.resolve("auto").unwrap();
        assert_eq!(
            path.to_str(),
            Some("/from/cli"),
            "CLI --session-path must win over SESSION_DIRECTORY env"
        );
    }

    #[test]
    fn spec_empty_returns_no_resolution() {
        let _g = env_test_lock();
        let prior = std::env::var(SESSION_DIRECTORY_ENV).ok();
        unsafe {
            std::env::remove_var(SESSION_DIRECTORY_ENV);
        }
        let spec = resolve_session_dir(&[]);
        if let Some(v) = prior {
            unsafe {
                std::env::set_var(SESSION_DIRECTORY_ENV, v);
            }
        }
        assert!(spec.is_empty());
        assert!(spec.resolve("auto").is_none());
    }

    #[test]
    fn spec_needs_auto_id_when_token_present() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session-path=/data/SESSION_x".into()];
        assert!(resolve_session_dir(&args).needs_auto_id());
    }

    #[test]
    fn spec_does_not_need_auto_id_when_path_is_concrete() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session-path=/data/specific".into()];
        assert!(!resolve_session_dir(&args).needs_auto_id());
    }

    #[test]
    fn spec_does_not_need_auto_id_with_explicit_name() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec![
            "--session-path=/data/SESSION_x".into(),
            "--session-name=alpha".into(),
        ];
        assert!(!resolve_session_dir(&args).needs_auto_id());
    }

    // -----------------------------------------------------------
    // Umbrella --session kv-list parsing
    // -----------------------------------------------------------

    #[test]
    fn umbrella_dir_shortcut_sets_path() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session=dir:asldkfjsldfj".into()];
        let (path, id) = resolve_session_dir(&args).resolve("auto").unwrap();
        assert_eq!(path.to_str(), Some("asldkfjsldfj"));
        assert_eq!(id, "asldkfjsldfj", "session id is the basename of the path");
    }

    #[test]
    fn umbrella_dir_with_subpath_yields_basename_id() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session=dir:l2k3j4/drr".into()];
        let (path, id) = resolve_session_dir(&args).resolve("auto").unwrap();
        assert_eq!(path.to_str(), Some("l2k3j4/drr"));
        assert_eq!(id, "drr");
    }

    #[test]
    fn umbrella_full_kv_list() {
        let _g = env_test_lock();
        clear_session_env();
        let args =
            vec!["--session=keep:42,name:sessname42,path:sessions/dir/SESSION,reuse:resume".into()];
        let spec = resolve_session_dir(&args);
        assert_eq!(spec.session_name.as_deref(), Some("sessname42"));
        assert_eq!(spec.session_path.as_deref(), Some("sessions/dir/SESSION"));
        assert_eq!(spec.reuse, SessionReuse::Resume);
        assert_eq!(spec.session_keep, 42);
        let (path, id) = spec.resolve("autogen").unwrap();
        assert_eq!(path.to_str(), Some("sessions/dir/sessname42"));
        assert_eq!(id, "sessname42");
    }

    #[test]
    fn umbrella_bare_restart_token_sets_reuse() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session=restart,dir:/tmp/x".into()];
        let spec = resolve_session_dir(&args);
        assert_eq!(spec.reuse, SessionReuse::Restart);
        assert_eq!(spec.session_path.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn umbrella_bare_resume_token_sets_reuse() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session=resume,name:foo".into()];
        let spec = resolve_session_dir(&args);
        assert_eq!(spec.reuse, SessionReuse::Resume);
        assert_eq!(spec.session_name.as_deref(), Some("foo"));
    }

    #[test]
    fn umbrella_long_form_overrides_umbrella() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec![
            "--session=name:from-umbrella".into(),
            "--session-name=from-longform".into(),
        ];
        let spec = resolve_session_dir(&args);
        assert_eq!(spec.session_name.as_deref(), Some("from-longform"));
    }

    #[test]
    fn umbrella_unknown_key_logs_warn_but_keeps_rest() {
        let _g = env_test_lock();
        clear_session_env();
        let args = vec!["--session=name:foo,what:nope,reuse:restart".into()];
        let spec = resolve_session_dir(&args);
        // Unknown 'what:nope' was logged + skipped; recognized
        // entries still applied.
        assert_eq!(spec.session_name.as_deref(), Some("foo"));
        assert_eq!(spec.reuse, SessionReuse::Restart);
    }

    // ── check_session_path: catch the `scenario=foo`-as-path footgun ──

    #[test]
    fn check_session_path_rejects_workload_param_shape() {
        // The classic recurring footgun: `--session-path scenario=foo`
        // would silently create `<cwd>/scenario=foo/...` because the
        // umbrella parser splits only on `:`.
        for bad in &[
            "scenario=foo",
            "scenario=/tmp/foo",
            "scenario=target",
            "scenario=target/test-tmp/x",
            "k=v",
            "key_with_underscore=value",
            "kebab-case=value",
        ] {
            assert!(
                check_session_path(bad, "test").is_err(),
                "should reject '{bad}'"
            );
        }
    }

    // ── SRD-77 session-path helpers ──────────────────────
    // Pin the structural composition: every helper builds on
    // `default_sessions_root()` so a future env-aware redirect
    // (cargo tmp, --session-path override, etc.) propagates
    // cleanly without each helper carrying its own literal.

    #[test]
    fn latest_session_dir_is_under_default_sessions_root() {
        let root = default_sessions_root();
        let latest = latest_session_dir();
        assert!(
            latest.starts_with(&root),
            "latest_session_dir MUST live under default_sessions_root \
             so the env-aware redirect (cargo tmp, etc.) covers it; \
             root={root:?}, latest={latest:?}"
        );
        assert_eq!(latest.file_name().and_then(|s| s.to_str()), Some("latest"));
    }

    #[test]
    fn latest_metrics_db_is_under_latest_session_dir() {
        let metrics = latest_metrics_db();
        let latest = latest_session_dir();
        assert!(
            metrics.starts_with(&latest),
            "latest_metrics_db MUST live under latest_session_dir; \
             metrics={metrics:?}, latest={latest:?}"
        );
        assert_eq!(
            metrics.file_name().and_then(|s| s.to_str()),
            Some("metrics.db")
        );
    }

    #[test]
    fn latest_session_log_is_under_latest_session_dir() {
        let log = latest_session_log();
        let latest = latest_session_dir();
        assert!(log.starts_with(&latest));
        assert_eq!(
            log.file_name().and_then(|s| s.to_str()),
            Some("session.log")
        );
    }

    #[test]
    fn latest_checkpoint_jsonl_is_under_latest_session_dir() {
        let ckpt = latest_checkpoint_jsonl();
        let latest = latest_session_dir();
        assert!(ckpt.starts_with(&latest));
        assert_eq!(
            ckpt.file_name().and_then(|s| s.to_str()),
            Some("checkpoint.jsonl")
        );
    }

    #[test]
    fn session_dir_named_is_a_sibling_of_latest() {
        let root = default_sessions_root();
        let named = session_dir_named("default_20260601_120000");
        assert!(named.starts_with(&root));
        assert_eq!(
            named.file_name().and_then(|s| s.to_str()),
            Some("default_20260601_120000"),
        );
    }

    #[test]
    fn check_session_path_accepts_real_paths() {
        for good in &[
            "/tmp/foo",
            "/tmp/foo/bar",
            "logs/session_2026",
            "./local/x",
            "../sibling/y",
            "relative/path",
            "/var/run/x=y",     // `=` inside path, not at the head
            "C:/Windows/maybe", // exotic but harmless
            "logs/SESSION/x",
        ] {
            assert!(
                check_session_path(good, "test").is_ok(),
                "should accept '{good}'"
            );
        }
    }

    #[test]
    fn check_session_path_message_names_remediation() {
        // The error must point the user at the right flag form,
        // not just say "bad path".
        let err = check_session_path("scenario=foo", "test").unwrap_err();
        assert!(err.contains("--session-path"), "missing flag hint: {err}");
        assert!(
            err.contains(":") && err.contains("kv separator"),
            "missing umbrella-form hint: {err}"
        );
    }

    #[test]
    fn check_session_path_empty_head_passes() {
        // `=value` (empty head) is unusual but doesn't match the
        // param shape — let it through; downstream path APIs will
        // reject it on their own terms.
        assert!(check_session_path("=foo", "test").is_ok());
    }

    #[test]
    fn flag_env_name_canonicalisation() {
        assert_eq!(flag_env_name("--session"), "NMBRS_SESSION");
        assert_eq!(flag_env_name("--session-name"), "NMBRS_SESSION_NAME");
        assert_eq!(flag_env_name("--session-path"), "NMBRS_SESSION_PATH");
        assert_eq!(flag_env_name("--multi-word-flag"), "NMBRS_MULTI_WORD_FLAG");
    }

    #[test]
    fn resolve_flag_picks_cli_when_only_cli_set() {
        let _g = env_test_lock();
        unsafe {
            std::env::remove_var("NMBRS_SESSION_NAME");
        }
        let args = vec!["--session-name=foo".into()];
        assert_eq!(
            resolve_flag(&args, "--session-name").as_deref(),
            Some("foo")
        );
    }

    #[test]
    fn resolve_flag_picks_env_when_only_env_set() {
        let _g = env_test_lock();
        unsafe {
            std::env::set_var("NMBRS_SESSION_NAME", "bar");
        }
        let v = resolve_flag(&[], "--session-name");
        unsafe {
            std::env::remove_var("NMBRS_SESSION_NAME");
        }
        assert_eq!(v.as_deref(), Some("bar"));
    }

    #[test]
    fn forecast_keep_purge_counts_excess() {
        let parent = std::env::temp_dir().join(format!(
            "nmbrs-forecast-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&parent).unwrap();

        // 5 dirs present, keep=10 → next run wouldn't purge.
        for i in 0..5 {
            std::fs::create_dir(parent.join(format!("s{i}"))).unwrap();
        }
        assert_eq!(forecast_keep_purge(&parent, 10), 0);

        // 5 dirs present, keep=5 → next run would purge 1.
        assert_eq!(forecast_keep_purge(&parent, 5), 1);

        // 5 dirs present, keep=3 → next run would purge 3.
        assert_eq!(forecast_keep_purge(&parent, 3), 3);

        // keep=0 disables.
        assert_eq!(forecast_keep_purge(&parent, 0), 0);

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn resolve_flag_returns_none_when_neither_set() {
        let _g = env_test_lock();
        unsafe {
            std::env::remove_var("NMBRS_SESSION_NAME");
        }
        assert!(resolve_flag(&[], "--session-name").is_none());
    }
    // Note: the conflict-error path (both CLI and env set)
    // calls process::exit(2). Testing it requires spawning a
    // subprocess; left to the e2e level.

    #[test]
    fn parse_duration_units() {
        use std::time::Duration;
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("3d").unwrap(), Duration::from_secs(259200));
        assert_eq!(parse_duration("4w").unwrap(), Duration::from_secs(2419200));
        // Bare integer = seconds.
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("not-a-number").is_err());
        assert!(parse_duration("abc4w").is_err());
    }

    #[test]
    fn purge_keeps_latest_n_sessions() {
        let parent = std::env::temp_dir().join(format!(
            "nmbrs-purge-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&parent).unwrap();

        // Create 5 dirs with staggered mtimes.
        for i in 0..5 {
            let d = parent.join(format!("sess_{i}"));
            std::fs::create_dir(&d).unwrap();
            let now = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * (5 - i));
            // Touch via filetime-style: re-create a marker so mtime
            // reflects roughly the right ordering.
            std::fs::write(d.join("metrics.db"), "x").unwrap();
            // Set the dir's modified time via a fresh file write; OS
            // updates mtime as side effect. For test stability, sort
            // order will follow the creation order, which is
            // newest-last.
            let _ = now;
        }

        // Newest 2 should survive after purge with max=2.
        purge_stale_sessions(&parent, 2, std::time::Duration::ZERO);

        let surviving: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        assert!(
            surviving.len() <= 2,
            "expected ≤2 survivors, got {}: {:?}",
            surviving.len(),
            surviving
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn purge_skips_logs_latest_symlink() {
        let parent = std::env::temp_dir().join(format!(
            "nmbrs-purge-symlink-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&parent).unwrap();
        let active = parent.join("active_session");
        std::fs::create_dir(&active).unwrap();
        std::fs::write(active.join("metrics.db"), "x").unwrap();
        // Create logs/latest symlink → active. Windows only allows
        // symlink creation with Developer Mode / admin — skip the
        // test rather than fail when the link can't exist at all.
        if symlink_any(&active, &parent.join("latest")).is_err() {
            eprintln!("skipping: cannot create symlinks here");
            let _ = std::fs::remove_dir_all(&parent);
            return;
        }

        // Purge with aggressive caps — `active` must not be deleted
        // because it's the symlink target.
        purge_stale_sessions(&parent, 0, std::time::Duration::from_secs(1));

        assert!(
            active.exists(),
            "active session pointed-at by logs/latest must survive purge"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }
}

#[cfg(test)]
mod confine_tests {
    use super::confine_to_dir;
    use std::path::Path;

    #[test]
    fn relative_paths_land_inside_the_base() {
        let base = Path::new("/sess");
        assert_eq!(
            confine_to_dir(base, "traces.jsonl").unwrap(),
            Path::new("/sess/traces.jsonl")
        );
        assert_eq!(
            confine_to_dir(base, "./sub/x.log").unwrap(),
            Path::new("/sess/./sub/x.log")
        );
    }

    #[test]
    fn escapes_and_absolutes_are_refused() {
        let base = Path::new("/sess");
        assert!(confine_to_dir(base, "../x").unwrap_err().contains(".."));
        assert!(
            confine_to_dir(base, "a/../../x")
                .unwrap_err()
                .contains("..")
        );
        assert!(
            confine_to_dir(base, "/etc/passwd")
                .unwrap_err()
                .contains("absolute")
        );
        assert!(confine_to_dir(base, "").unwrap_err().contains("empty"));
    }
}
