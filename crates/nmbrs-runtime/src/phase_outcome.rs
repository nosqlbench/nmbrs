// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-76 — Phase Outcome Disposition.
//!
//! Structured per-phase terminal-state record. One canonical
//! shape carries both the per-phase status (Completed /
//! Failed / Skipped / CursorSuspended) and the chronological
//! error list, so the realtime status surface AND `nmbrs
//! replay` consume from one projection.
//!
//! This module is Push 1 of the SRD-76 migration plan: it
//! defines the data model and provides the no-side-effects
//! API. Push 2 wires the executor's per-phase population;
//! Push 3 adds sqlite persistence; Push 4 adds the new
//! Readouts; Push 5 wires `nmbrs replay`.
//!
//! Cross-refs:
//! - SRD-03 §"Error scoping" — error-class strings here
//!   match the strings the `errors:` policy routes on.
//! - SRD-44 §"Phase identity" — `PhaseIdentity` matches the
//!   checkpoint writer's identity tuple.
//! - SRD-68 §"I-6 Workload-load pre-flight is non-mutating"
//!   — `op_template` is the operator's pristine YAML
//!   verbatim; `op_resolved` is the wire-rendered form.
//! - SRD-63 — The Readout layer that will render this in
//!   Push 4.
//! - SRD-75 — Phase-poll; `poll_timeout` is one of the
//!   error classes that lands here.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// SRD-82 Part 1 — **how much** of a shell's work ran. Orthogonal to
/// [`Validity`]: a result can be `Interrupted` yet `Succeeded`
/// (re-usable partial progress) or `Completed` yet `Failed` (ran fully
/// but the result is garbage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// In flight (not a terminal outcome).
    Running,
    /// Ran to its natural end.
    Completed,
    /// Stopped before its natural end (a signal, a stop condition).
    Interrupted,
    /// Never started (filtered / unreached / resume-skipped).
    Skipped,
}

/// SRD-82 Part 1 — whether the produced result is **usable**.
/// Orthogonal to [`Disposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validity {
    /// The result is trustworthy.
    Succeeded,
    /// The result is not trustworthy — do not rely on it.
    Failed,
}

/// SRD-92 / ExecUnification — the opaque return **payload** a unit may carry
/// on its [`Outcome`], above the leaf-projection boundary.
///
/// A scaffold-owned **marker** (no methods yet — the loop/poll read-path is a
/// later step): the op leaf fills it, aggregates leave it `None`. Kept neutral
/// (not the adapter's [`crate::adapter::ResultBody`]) so the universal return
/// type does not depend on the adapter result trait, and a future aggregate
/// fold-summary can implement `Payload` honestly without faking row-count
/// methods. `Send + Sync + 'static` keeps [`Outcome`] spawn / `ArcSwap` /
/// `Mutex` safe; `Debug` keeps `Outcome`'s derive working.
pub trait Payload: Send + Sync + std::fmt::Debug + 'static {}

/// Every adapter [`crate::adapter::ResultBody`] is a [`Payload`]. A **blanket**
/// impl (not a subtrait) so it covers `ResultBody` impls in the adapter crates
/// and tests too with **zero cross-crate churn** — the impl lives here, where
/// `Payload` is defined. A neutral fold-summary type may still `impl Payload`
/// directly (no overlap, since it is not a `ResultBody`). Trade-off: a blanket
/// gives no `dyn`-upcasting, so Step 2 lifts the *erased* `Box<dyn ResultBody>`
/// via a thin wrapper rather than an `Arc<dyn ResultBody> -> Arc<dyn Payload>`
/// coercion.
impl<T: ?Sized + crate::adapter::ResultBody + 'static> Payload for T {}

/// SRD-82 Part 1 — the two-axis result of any execution shell, and the
/// `effect` of an SRD-83 stop condition. The four meaningful
/// quadrants: `Completed+Succeeded` (clean), `Completed+Failed` (ran
/// fully, garbage), `Interrupted+Succeeded` (partial, re-usable),
/// `Interrupted+Failed` (partial, discard).
///
/// `PartialEq` is hand-written (control facts only); `Eq` is dropped — see the
/// `impl PartialEq` below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub disposition: Disposition,
    pub validity: Validity,
    /// SRD-92 — optional human reason (the first failing child's message),
    /// absorbing the parallel `(Outcome, Option<String>)` tuple the scenario
    /// shell carries today. `None` for clean outcomes; set via
    /// [`Outcome::with_reason`]. Dropping `Copy` (a `String` is not `Copy`)
    /// is the cost — by-value reuses become moves/clones. Serialized only
    /// when present, so axis-only `Outcome` JSON round-trips unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// SRD-92 / ExecUnification — the optional opaque return **payload** a unit
    /// may carry above the leaf-projection boundary (the op leaf fills it via
    /// [`Outcome::with_payload`]; aggregates leave it `None`). `Arc` (not `Box`)
    /// so `Outcome` stays `Clone`; `#[serde(skip)]` because it is runtime-only,
    /// so axis-only JSON round-trips unchanged. Excluded from `PartialEq` — it
    /// is data, not control identity. Its *contents* are the deferred data layer.
    #[serde(skip)]
    pub payload: Option<Arc<dyn Payload>>,
}

impl Outcome {
    pub const fn new(disposition: Disposition, validity: Validity) -> Self {
        Self {
            disposition,
            validity,
            reason: None,
            payload: None,
        }
    }
    /// Ran fully, result trustworthy.
    pub const fn completed() -> Self {
        Self::new(Disposition::Completed, Validity::Succeeded)
    }
    /// Stopped early, result untrustworthy — the SRD-83 `fail` effect
    /// / a `StopCause::Fault`.
    pub const fn failed() -> Self {
        Self::new(Disposition::Interrupted, Validity::Failed)
    }
    /// SRD-92 — ran fully but the result is untrustworthy (a ran-and-errored
    /// op): `Completed+Failed`. Distinct from [`Outcome::failed`]
    /// (`Interrupted+Failed` — stopped early by a fault).
    pub const fn completed_failed() -> Self {
        Self::new(Disposition::Completed, Validity::Failed)
    }
    /// Stopped early, partial result usable — the SRD-83 `stop` effect
    /// / a `StopCause::Interrupt` (e.g. user Ctrl-C, budget met).
    pub const fn interrupted() -> Self {
        Self::new(Disposition::Interrupted, Validity::Succeeded)
    }
    /// Never started.
    pub const fn skipped() -> Self {
        Self::new(Disposition::Skipped, Validity::Succeeded)
    }

    /// SRD-92 — attach the human reason (the failing child's message). Used
    /// by the flow-Outcome-up step so the leaf shells / aggregate fold carry
    /// the message that the `?`-propagation path needs, retiring the parallel
    /// `(Outcome, Option<String>)` tuple.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Binary pass/fail projection for session-level counting: the
    /// only red mark is an untrustworthy result. `Interrupted +
    /// Succeeded` (Ctrl-C, budget met, cursor-suspend) and `Skipped`
    /// are non-failures from the operator's perspective.
    pub fn is_failure(&self) -> bool {
        matches!(self.validity, Validity::Failed)
    }

    /// Glyph for compact rendering. One per meaningful axis pair
    /// (SRD-82 Part 1): ✓ ran-and-trustworthy, ✗ untrustworthy
    /// (whether it ran fully or not), … re-usable partial progress,
    /// ~ never started, ⋯ still in flight.
    pub fn glyph(&self) -> char {
        match (self.disposition, self.validity) {
            (Disposition::Running, _) => '⋯',
            (Disposition::Skipped, _) => '~',
            (_, Validity::Failed) => '✗',
            (Disposition::Completed, Validity::Succeeded) => '✓',
            (Disposition::Interrupted, Validity::Succeeded) => '…',
        }
    }

    /// All-lowercase short label, stable for log lines / CI greps /
    /// the sqlite `status` column. The axes yield five terminal
    /// labels; `interrupted` subsumes the retired `cursor_suspended`
    /// (a resume cursor on the outcome carries the resumability
    /// signal — SRD-82 Part 1).
    pub fn label(&self) -> &'static str {
        match (self.disposition, self.validity) {
            (Disposition::Running, _) => "running",
            (Disposition::Skipped, _) => "skipped",
            (Disposition::Completed, Validity::Succeeded) => "completed",
            (Disposition::Completed, Validity::Failed) => "completed_failed",
            (Disposition::Interrupted, Validity::Succeeded) => "interrupted",
            (Disposition::Interrupted, Validity::Failed) => "failed",
        }
    }

    /// SRD-92 / ExecUnification — attach the opaque return [`Payload`] (the op
    /// leaf's body, lifted at the projection boundary in a later step).
    /// Aggregates never call this — the payload rides the leaf's `Outcome` and
    /// is ignored by the fold.
    pub fn with_payload(mut self, payload: Arc<dyn Payload>) -> Self {
        self.payload = Some(payload);
        self
    }
}

/// SRD-92 / ExecUnification — equality over the CONTROL FACTS only
/// (`disposition`, `validity`, `reason`). The `payload` is intentionally
/// excluded: it is data, not control identity, so two outcomes that differ
/// only in their payload compare equal. Do NOT "fix" this to compare payloads —
/// `dyn Payload` is not `PartialEq`, and aggregation / tests key on control
/// facts, never the payload. (`Eq` is dropped: it is unused — no
/// `HashSet`/`HashMap<Outcome>` exists — and a `dyn` field can't satisfy it.)
impl PartialEq for Outcome {
    fn eq(&self, other: &Self) -> bool {
        self.disposition == other.disposition
            && self.validity == other.validity
            && self.reason == other.reason
    }
}

/// Session-wide pass/fail disposition. Projected from the
/// per-phase statuses by walking every populated
/// [`PhaseOutcome`] on the scene tree.
///
/// `SessionDisposition` is the single source of truth for
/// "what happened?" — drives the process exit code, the
/// terminating status line, the `nmbrs replay` header, and
/// the (future) `session_disposition` readout in
/// SRD-63's `on_session_end` slot.
///
/// SRD-76 §"SessionDisposition" — past callers each did
/// their own ad-hoc walk over phase state with subtly
/// different rules (was Skipped a pass? was Ctrl-C a
/// failure?). This enum centralises the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDisposition {
    /// Every phase that ran terminated cleanly
    /// (Completed, Skipped, or CursorSuspended).
    /// Interrupted-via-signal sessions where no phase
    /// actually failed land here — interrupted ≠ failed
    /// from the operator's perspective.
    Success,
    /// At least one phase's outcome carries `Validity::Failed`. The
    /// realtime status surface and `nmbrs replay` render
    /// this in red; CI / scripted callers observe via the
    /// process exit code (non-zero) and the JSON output.
    Failure,
}

impl SessionDisposition {
    /// Process exit code for this disposition. 0 for
    /// success; non-zero (today `1`) for failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            SessionDisposition::Success => 0,
            SessionDisposition::Failure => 1,
        }
    }

    /// Uppercase short label suitable for the terminating
    /// status line: `session: idx_sweep (SUCCESS in …)`
    /// vs. `session: idx_sweep (FAILURE: N phases failed)`.
    pub fn label(&self) -> &'static str {
        match self {
            SessionDisposition::Success => "SUCCESS",
            SessionDisposition::Failure => "FAILURE",
        }
    }
}

/// Phase identity tuple per SRD-44 §"Phase identity" —
/// `(name, labels)`. The combination is unique within a
/// session: even sweep cells that share a phase name have
/// distinct labels (the striated coord path).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PhaseIdentity {
    pub name: String,
    /// Striated label path — the `(profile=…), (sm=…), …`
    /// form the executor produces via
    /// `format_scope_coordinate_path`. Empty for
    /// non-iter phases.
    pub labels: String,
}

impl PhaseIdentity {
    pub fn new(name: impl Into<String>, labels: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            labels: labels.into(),
        }
    }
}

/// One error recorded against a phase. Captures the
/// originating cycle (when an op error), the dispenser's
/// pristine + resolved op text (per SRD-68), and the
/// class string the `errors:` policy matches on.
///
/// `op_name` / `cycle` / `op_template` / `op_resolved`
/// are `None` when the error originates outside any
/// dispenser — workload-load validation failures,
/// phase-poll deadlines (SRD-75), missing-adapter init
/// errors, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseErrorDetail {
    /// Error class string (matches the `errors:` policy
    /// vocabulary): `Timeout`, `cql_error`, `poll_timeout`,
    /// `validate_failure`, `BindError`, …
    pub class: String,
    /// Human-readable message — the detail an operator
    /// needs to act. Multi-line allowed.
    pub message: String,
    /// Op identity (template name) that triggered this
    /// error. `None` for phase-level errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_name: Option<String>,
    /// Cycle number for op errors. `None` for
    /// phase-level errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<u64>,
    /// Pristine op-template text per SRD-68 §"I-6".
    /// Operator's YAML verbatim, with `{name}` placeholders
    /// intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_template: Option<String>,
    /// Wire-rendered op text for the failing cycle. What
    /// the adapter actually sent. `None` when no render
    /// attempt happened (pre-dispense errors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_resolved: Option<String>,
    /// Nanos-since-epoch when the error was recorded.
    /// Used for chronological replay rendering and
    /// cross-correlation with metric snapshots.
    pub at_nanos: u64,
    /// Whether the underlying error was classified as
    /// retryable. Useful for diagnostics: a workload with
    /// 100 retryable errors reads differently from one
    /// with 1 fatal error.
    #[serde(default)]
    pub retryable: bool,
}

/// SRD-44 cursor-resume state. Stub for Push 1 (the
/// resume machinery is independent of this SRD); future
/// pushes populate it with the actual restart-cursor
/// payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResumeCursor {
    /// Opaque source-factory state to resume from. The
    /// concrete shape is owned by SRD-44's checkpoint
    /// layer; SRD-76 just carries it through. Empty when
    /// no resume state is recorded.
    #[serde(default)]
    pub opaque: Vec<u8>,
}

/// SRD-76 — complete description of how a single phase
/// ended. Built once at phase end by the executor,
/// installed on the scene tree, optionally persisted to
/// sqlite (Push 3), and rendered via the SRD-76 readouts
/// (Push 4).
/// SRD-83 (C3) — the closed vocabulary of failure-reason classes on a
/// phase outcome. Derived from the first `PhaseErrorDetail.class` (see
/// [`PhaseOutcome::reason_class`]); the stable strings below are the
/// sqlite `reason_class` column values and the report-layer grouping
/// keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonClass {
    /// A governance `timeout:` (or an SRD-75 poll timeout) expired —
    /// the protocol OUT-OF-RANGE disposition.
    Timeout,
    /// A declared `stop_when` condition with a failing effect tripped
    /// (includes the synthesized `error_rate_exceeded` guard).
    StopCondition,
    /// Op/dispatch errors stopped the phase.
    Error,
    /// A panic was caught and routed as an error.
    Panic,
    /// An operator action (Ctrl-C / stop request) cut the phase.
    Operator,
}

impl ReasonClass {
    /// The stable serialized token (sqlite column value, report key).
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasonClass::Timeout => "timeout",
            ReasonClass::StopCondition => "stop_condition",
            ReasonClass::Error => "error",
            ReasonClass::Panic => "panic",
            ReasonClass::Operator => "operator",
        }
    }

    /// Classify an error-class string from the trip/error paths. The
    /// class strings are the established vocabulary: `timeout` (the
    /// synthesized governance guard), `poll_timeout` (SRD-75),
    /// `panic` (wrapper-caught), `stop_condition: <when>` /
    /// `error_rate_exceeded` (SRD-83 trips), `operator` (session
    /// stop). Anything else is a plain error.
    pub fn from_error_class(class: &str) -> Self {
        match class {
            "timeout" | "poll_timeout" => ReasonClass::Timeout,
            "panic" => ReasonClass::Panic,
            "operator" => ReasonClass::Operator,
            "error_rate_exceeded" => ReasonClass::StopCondition,
            c if c.starts_with("stop_condition") => ReasonClass::StopCondition,
            _ => ReasonClass::Error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "PhaseOutcomeWire")]
pub struct PhaseOutcome {
    pub phase_id: PhaseIdentity,
    /// SRD-82 Part 1 — the two axes ARE the stored canonical (the
    /// storage migration the old `PhaseStatus::to_outcome` doc
    /// promised). Legacy checkpoint records that carried a single
    /// `status` string still deserialize via [`PhaseOutcomeWire`].
    pub disposition: Disposition,
    pub validity: Validity,
    /// Wall-clock duration from `phase_starting` to this
    /// outcome being recorded. `0.0` for skipped phases.
    pub duration_secs: f64,
    /// Errors collected during the phase. Empty for
    /// `Completed` / `Skipped`. Non-empty for `Failed`.
    /// Chronologically ordered by `at_nanos`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<PhaseErrorDetail>,
    /// Resume state for the next session. `None` when the
    /// phase doesn't support cursor-resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cursor: Option<ResumeCursor>,
    /// SRD-77 `--scope=changed` — the phase's provenance BASE
    /// hash (SRD-107: ancestor chain below the session node
    /// composed with the config digest; param values live in
    /// [`Self::params_consumed`] instead). Hex-encoded so the
    /// storage layer can round-trip as TEXT. `None` for
    /// outcomes recorded before the column was added (legacy
    /// rows) or for skipped phases that never computed their
    /// hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_hash: Option<String>,
    /// SRD-107 — the phase's consumed-params map as canonical
    /// JSON (`{"name":"<value sha256 hex>",…}`, sorted keys).
    /// Skip validity's per-param leg: current values of exactly
    /// these names must digest to these values. `None` on
    /// legacy rows (whose base hash never matches current
    /// formulas anyway) and on skipped phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_consumed: Option<String>,
}

/// Deserialization shape for [`PhaseOutcome`]: accepts BOTH the
/// canonical two-axis form and legacy checkpoint records that carried
/// the retired single `status` string. The checkpoint JSONL store is
/// event-sourced and never rewritten (SRD-44a), so records written
/// before the axes migration must stay readable — resume-skip
/// identity matching folds over them.
#[derive(Deserialize)]
struct PhaseOutcomeWire {
    phase_id: PhaseIdentity,
    disposition: Option<Disposition>,
    validity: Option<Validity>,
    /// Legacy single-status field (pre-migration records).
    status: Option<LegacyPhaseStatus>,
    duration_secs: f64,
    #[serde(default)]
    errors: Vec<PhaseErrorDetail>,
    #[serde(default)]
    resume_cursor: Option<ResumeCursor>,
    #[serde(default)]
    phase_hash: Option<String>,
    #[serde(default)]
    params_consumed: Option<String>,
}

/// The retired conflated status, kept ONLY as a deserialization
/// target for legacy records. `cursor_suspended` collapses to
/// `Interrupted + Succeeded` per SRD-82 Part 1 (the record's resume
/// cursor carries the resumability signal); legacy `failed` maps to
/// `Interrupted + Failed` (the single status couldn't distinguish it
/// from `Completed + Failed`).
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyPhaseStatus {
    Completed,
    Failed,
    Skipped,
    CursorSuspended,
}

impl From<PhaseOutcomeWire> for PhaseOutcome {
    fn from(w: PhaseOutcomeWire) -> Self {
        let (disposition, validity) = match (w.disposition, w.validity, w.status) {
            (Some(d), Some(v), _) => (d, v),
            (_, _, Some(LegacyPhaseStatus::Completed)) => {
                (Disposition::Completed, Validity::Succeeded)
            }
            (_, _, Some(LegacyPhaseStatus::Failed)) => (Disposition::Interrupted, Validity::Failed),
            (_, _, Some(LegacyPhaseStatus::Skipped)) => (Disposition::Skipped, Validity::Succeeded),
            (_, _, Some(LegacyPhaseStatus::CursorSuspended)) => {
                (Disposition::Interrupted, Validity::Succeeded)
            }
            // Neither form present: benign default (a record this
            // malformed predates both formats).
            _ => (Disposition::Completed, Validity::Succeeded),
        };
        Self {
            phase_id: w.phase_id,
            disposition,
            validity,
            duration_secs: w.duration_secs,
            errors: w.errors,
            resume_cursor: w.resume_cursor,
            phase_hash: w.phase_hash,
            params_consumed: w.params_consumed,
        }
    }
}

impl PhaseOutcome {
    /// Build a Completed outcome. Convenience for the
    /// happy path; errors must be empty.
    /// SRD-83 (C3) — machine-readable class of WHY this phase ended
    /// short of a usable result. DERIVED from the first error's class
    /// (single source of truth — the trip/error paths already stamp
    /// it), never stored: legacy outcomes classify identically.
    /// `None` for any Succeeded validity — a graceful stop or natural
    /// completion has no failure reason to classify.
    pub fn reason_class(&self) -> Option<ReasonClass> {
        match self.validity {
            Validity::Succeeded => None,
            Validity::Failed => Some(ReasonClass::from_error_class(
                self.errors
                    .first()
                    .map(|e| e.class.as_str())
                    .unwrap_or("error"),
            )),
        }
    }

    /// SRD-83 (C3) — the testing-protocol three-way disposition
    /// (17_/P0.5): COMPLETED (usable result, natural or graceful),
    /// OUT-OF-RANGE (a governance timeout expired — disqualified at
    /// this tier), FAILED (anything else), plus SKIPPED for
    /// resume-skipped phases. Model-level, replacing report-side
    /// string-matching conventions.
    pub fn protocol_class(&self) -> &'static str {
        if matches!(self.disposition, Disposition::Skipped) {
            return "SKIPPED";
        }
        match self.reason_class() {
            None => "COMPLETED",
            Some(ReasonClass::Timeout) => "OUT-OF-RANGE",
            Some(_) => "FAILED",
        }
    }

    pub fn completed(phase_id: PhaseIdentity, duration_secs: f64) -> Self {
        Self {
            phase_id,
            disposition: Disposition::Completed,
            validity: Validity::Succeeded,
            duration_secs,
            errors: Vec::new(),
            resume_cursor: None,
            phase_hash: None,
            params_consumed: None,
        }
    }

    /// Build a Failed outcome from a non-empty error list.
    /// Panics if `errors` is empty — `Failed` without an
    /// error is structurally invalid per SRD-76
    /// §"Invariants", in every build profile.
    pub fn failed(
        phase_id: PhaseIdentity,
        duration_secs: f64,
        errors: Vec<PhaseErrorDetail>,
    ) -> Self {
        assert!(
            !errors.is_empty(),
            "PhaseOutcome::failed requires at least one error"
        );
        Self {
            phase_id,
            disposition: Disposition::Interrupted,
            validity: Validity::Failed,
            duration_secs,
            errors,
            resume_cursor: None,
            phase_hash: None,
            params_consumed: None,
        }
    }

    /// Build a Skipped outcome. Used by SRD-44's
    /// resume-on-checkpoint skip path. Duration is `0.0`
    /// because no actual work was done.
    pub fn skipped(phase_id: PhaseIdentity) -> Self {
        Self {
            phase_id,
            disposition: Disposition::Skipped,
            validity: Validity::Succeeded,
            duration_secs: 0.0,
            errors: Vec::new(),
            resume_cursor: None,
            phase_hash: None,
            params_consumed: None,
        }
    }

    /// Build an Interrupted+Succeeded outcome — re-usable partial
    /// progress (a clean early halt, a cursor suspension). The resume
    /// cursor, when present, carries the resumability signal that the
    /// retired `CursorSuspended` status used to encode.
    pub fn interrupted(
        phase_id: PhaseIdentity,
        duration_secs: f64,
        resume_cursor: Option<ResumeCursor>,
    ) -> Self {
        Self {
            phase_id,
            disposition: Disposition::Interrupted,
            validity: Validity::Succeeded,
            duration_secs,
            errors: Vec::new(),
            resume_cursor,
            phase_hash: None,
            params_consumed: None,
        }
    }

    /// Stamp the GK chain-hash on this outcome. Builder-style
    /// so existing callers that don't yet have the hash
    /// available (legacy / partial paths) stay unchanged;
    /// SRD-77-aware callers chain `.completed(...).with_hash(h)`.
    pub fn with_phase_hash(mut self, hex_hash: String) -> Self {
        self.phase_hash = Some(hex_hash);
        self
    }

    /// Stamp the SRD-107 consumed-params JSON on this outcome.
    /// Builder-style, same rationale as [`Self::with_phase_hash`].
    pub fn with_params_consumed(mut self, json: Option<String>) -> Self {
        self.params_consumed = json;
        self
    }

    /// Convenience: the first error's message, or `None`
    /// when the phase didn't fail. Used by the compact
    /// renderer to give an at-a-glance reason.
    pub fn first_error_message(&self) -> Option<&str> {
        self.errors.first().map(|e| e.message.as_str())
    }

    /// SRD-82 Part 1 — the two-axis [`Outcome`], straight from the
    /// stored axes (they ARE the canonical since the storage
    /// migration).
    pub fn outcome(&self) -> Outcome {
        Outcome::new(self.disposition, self.validity)
    }

    /// See [`Outcome::is_failure`].
    pub fn is_failure(&self) -> bool {
        self.outcome().is_failure()
    }

    /// See [`Outcome::glyph`].
    pub fn glyph(&self) -> char {
        self.outcome().glyph()
    }

    /// See [`Outcome::label`].
    pub fn label(&self) -> &'static str {
        self.outcome().label()
    }

    /// SRD-76 Push 3 — project this outcome into the
    /// storage-layer row shape used by
    /// [`nmbrs_metrics::reporters::sqlite::SqliteReporter::write_phase_outcome`].
    /// `session` / `exec_id` come from the active session
    /// (SRD-77); `started_at_nanos` is supplied by the
    /// caller (the executor captures it at phase entry);
    /// `ended_at_nanos` is taken from `SystemTime::now()` so
    /// the on-disk row matches the wall-clock moment the
    /// outcome was sealed.
    pub fn to_sqlite_row(
        &self,
        session: &str,
        exec_id: u64,
        started_at_nanos: i64,
    ) -> nmbrs_metrics::reporters::sqlite::PhaseOutcomeRow {
        let ended_at_nanos: i64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(started_at_nanos);
        nmbrs_metrics::reporters::sqlite::PhaseOutcomeRow {
            session: session.to_string(),
            exec_id,
            phase_name: self.phase_id.name.clone(),
            phase_labels: self.phase_id.labels.clone(),
            status: self.label().to_string(),
            duration_secs: self.duration_secs,
            started_at_nanos,
            ended_at_nanos,
            // SRD-83 (C3) — denormalized for report GROUP BY; the
            // derived accessor is the single source of truth.
            reason_class: self.reason_class().map(|c| c.as_str().to_string()),
            phase_hash: self.phase_hash.clone(),
            params_consumed: self.params_consumed.clone(),
            errors: self
                .errors
                .iter()
                .map(|e| nmbrs_metrics::reporters::sqlite::PhaseErrorRow {
                    class: e.class.clone(),
                    message: e.message.clone(),
                    op_name: e.op_name.clone(),
                    cycle: e.cycle,
                    op_template: e.op_template.clone(),
                    op_resolved: e.op_resolved.clone(),
                    at_nanos: e.at_nanos as i64,
                    retryable: e.retryable,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_failure_keys_on_validity_alone() {
        // SRD-82 Part 1: the red mark is an untrustworthy result,
        // regardless of how much of the work ran.
        assert!(!Outcome::completed().is_failure());
        assert!(Outcome::failed().is_failure());
        assert!(Outcome::completed_failed().is_failure());
        assert!(!Outcome::interrupted().is_failure());
        assert!(!Outcome::skipped().is_failure());
    }

    #[test]
    fn glyphs_and_labels_cover_the_axis_pairs() {
        assert_eq!(Outcome::completed().glyph(), '✓');
        assert_eq!(Outcome::failed().glyph(), '✗');
        assert_eq!(Outcome::completed_failed().glyph(), '✗');
        assert_eq!(Outcome::interrupted().glyph(), '…');
        assert_eq!(Outcome::skipped().glyph(), '~');
        assert_eq!(Outcome::completed().label(), "completed");
        assert_eq!(Outcome::failed().label(), "failed");
        assert_eq!(Outcome::completed_failed().label(), "completed_failed");
        assert_eq!(Outcome::interrupted().label(), "interrupted");
        assert_eq!(Outcome::skipped().label(), "skipped");
    }

    #[test]
    fn session_disposition_exit_codes() {
        assert_eq!(SessionDisposition::Success.exit_code(), 0);
        assert_ne!(SessionDisposition::Failure.exit_code(), 0);
    }

    #[test]
    fn outcome_completed_has_no_errors() {
        let o = PhaseOutcome::completed(PhaseIdentity::new("p", "x=1"), 1.5);
        assert_eq!(o.disposition, Disposition::Completed);
        assert_eq!(o.validity, Validity::Succeeded);
        assert!(o.errors.is_empty());
        assert!(o.first_error_message().is_none());
    }

    #[test]
    fn outcome_failed_carries_errors() {
        let errors = vec![PhaseErrorDetail {
            class: "Timeout".into(),
            message: "connection timed out".into(),
            op_name: Some("read_state".into()),
            cycle: Some(0),
            op_template: Some("SELECT ...".into()),
            op_resolved: Some("SELECT * FROM ks.t".into()),
            at_nanos: 1_000_000_000,
            retryable: true,
        }];
        let o = PhaseOutcome::failed(PhaseIdentity::new("p", "x=1"), 30.0, errors.clone());
        assert_eq!(o.disposition, Disposition::Interrupted);
        assert_eq!(o.validity, Validity::Failed);
        assert_eq!(o.errors, errors);
        assert_eq!(o.first_error_message(), Some("connection timed out"));
    }

    #[test]
    #[should_panic(expected = "at least one error")]
    fn outcome_failed_requires_non_empty_errors() {
        let _ = PhaseOutcome::failed(PhaseIdentity::new("p", ""), 1.0, Vec::new());
    }

    #[test]
    fn outcome_skipped_has_zero_duration_and_no_errors() {
        let o = PhaseOutcome::skipped(PhaseIdentity::new("p", ""));
        assert_eq!(o.disposition, Disposition::Skipped);
        assert_eq!(o.duration_secs, 0.0);
        assert!(o.errors.is_empty());
    }

    /// Round-trip serde so the persistence layer (Push 3
    /// sqlite, JSON in `nmbrs replay --json`) sees a
    /// stable shape.
    #[test]
    fn outcome_round_trips_through_json() {
        let original = PhaseOutcome {
            phase_id: PhaseIdentity::new("ensure_compacted", "(k=10)"),
            disposition: Disposition::Interrupted,
            validity: Validity::Failed,
            duration_secs: 14400.0,
            errors: vec![PhaseErrorDetail {
                class: "poll_timeout".into(),
                message: "deadline reached".into(),
                op_name: None,
                cycle: None,
                op_template: None,
                op_resolved: None,
                at_nanos: 0,
                retryable: false,
            }],
            resume_cursor: None,
            phase_hash: None,
            params_consumed: None,
        };
        let json = serde_json::to_string(&original).expect("serialise");
        let parsed: PhaseOutcome = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(parsed, original);
    }

    #[test]
    fn two_axis_outcome_projects_to_and_from_status() {
        // The four constructors land in the right quadrants.
        assert_eq!(
            Outcome::completed(),
            Outcome::new(Disposition::Completed, Validity::Succeeded)
        );
        assert_eq!(
            Outcome::failed(),
            Outcome::new(Disposition::Interrupted, Validity::Failed)
        );
        assert_eq!(
            Outcome::interrupted(),
            Outcome::new(Disposition::Interrupted, Validity::Succeeded)
        );

        // Validity drives is_failure, orthogonal to disposition.
        assert!(Outcome::failed().is_failure());
        assert!(!Outcome::interrupted().is_failure());
        assert!(!Outcome::completed().is_failure());

        // PhaseOutcome surfaces the two-axis view.
        let oc = PhaseOutcome::completed(PhaseIdentity::new("p", ""), 1.0);
        assert_eq!(oc.outcome(), Outcome::completed());
        assert!(!oc.outcome().is_failure());
    }

    /// The checkpoint JSONL store is event-sourced and never
    /// rewritten (SRD-44a): records written before the axes
    /// migration carry a single `status` string and MUST keep
    /// deserializing — resume-skip identity matching folds over
    /// them. `cursor_suspended` collapses to Interrupted+Succeeded.
    #[test]
    fn legacy_status_records_still_deserialize() {
        let cases = [
            ("completed", Disposition::Completed, Validity::Succeeded),
            ("failed", Disposition::Interrupted, Validity::Failed),
            ("skipped", Disposition::Skipped, Validity::Succeeded),
            (
                "cursor_suspended",
                Disposition::Interrupted,
                Validity::Succeeded,
            ),
        ];
        for (status, d, v) in cases {
            let json = format!(
                r#"{{"phase_id":{{"name":"p","labels":""}},"status":"{status}","duration_secs":1.0}}"#
            );
            let parsed: PhaseOutcome = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("legacy '{status}' must parse: {e}"));
            assert_eq!(parsed.disposition, d, "disposition for '{status}'");
            assert_eq!(parsed.validity, v, "validity for '{status}'");
        }
    }
}
