// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! [`ReadoutBinder`] — runtime adapter between the static
//! workload binding and the live display surface. See
//! SRD-63 §7.
//!
//! Push 3 ships:
//!
//! - The trait surface ([`ReadoutBinder`], [`ReadoutSink`],
//!   [`LayoutHint`], [`BinderKey`], [`BakedBody`],
//!   [`RenderStep`]).
//! - A stateless [`DefaultBinder`] that walks each event's
//!   bindings in order and renders. Stateful interactive
//!   variants (focus highlight, LOD overrides,
//!   overlay-held flag) land in Push 5.
//! - A line-buffer [`StringSink`] that the terminal-mode
//!   surface uses; the TUI gets its own `Vec<Span>` sink in
//!   Push 5.

use std::collections::HashMap;
use std::sync::Arc;

use super::buf::StringBuf;
use super::context::ReadoutContext;
use super::readout::{ContentMode, Lod, Readout, ReadoutOptions};
use crate::lifecycle::EventType;

/// Reference-counted readout handle. The `Registry` returns
/// these by wrapping unit-struct builtins in `Arc::new`;
/// per-workload custom readouts (planned for Push 4 of the
/// SRD-63 follow-on work) ride the same shape. Cheap to
/// clone — refcount bump only — and hands a `&dyn Readout`
/// out via `as_ref` for the actual render call.
pub type ReadoutHandle = Arc<dyn Readout>;

/// One step in a baked readout body. Either a literal run
/// of text, a render call against a registered readout, or
/// a colour / style directive (Push 4) that wraps the next
/// step in ANSI on/off bytes.
pub enum RenderStep {
    /// Literal text emitted verbatim (quoted strings,
    /// punctuation between readout calls, joining
    /// whitespace).
    Literal(String),
    /// Render a registered readout with the resolved
    /// options, LOD, and content mode.
    Render {
        readout: ReadoutHandle,
        lod: Lod,
        layout: LayoutMode,
        options: ReadoutOptions,
        /// Per-call colour / style override (from
        /// `color=` / `style=` options). The binder wraps
        /// the readout's render in an ANSI on/off pair
        /// when set.
        color: Option<crate::readouts::color::ColorSpec>,
    },
    /// Inline colour / style directive (`@RED`, `[#hex]`,
    /// `@INFO`). Single-shot: applies to the next non-
    /// directive step only. The binder emits ANSI on
    /// before that step and off after it.
    ColorDirective(crate::readouts::color::ColorSpec),
}

impl std::fmt::Debug for RenderStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderStep::Literal(s) => write!(f, "Literal({s:?})"),
            RenderStep::Render {
                readout,
                lod,
                layout,
                color,
                ..
            } => {
                write!(
                    f,
                    "Render {{ name: {:?}, lod: {lod:?}, layout: {layout:?}, color: {color:?} }}",
                    readout.name()
                )
            }
            RenderStep::ColorDirective(c) => {
                write!(f, "ColorDirective({c:?})")
            }
        }
    }
}

/// Layout intent expressed inside a readout body via the
/// `layout=` option. See SRD-63 §5.3.1. The binder maps
/// this to a [`LayoutHint`] for the sink at render time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// Default — pick per LOD: compact ⇒ inline,
    /// labeled / expanded ⇒ block.
    #[default]
    Auto,
    /// Force inline regardless of LOD.
    Inline,
    /// Force block regardless of LOD.
    Block,
}

/// What the binder writes to the sink for a single render
/// step. The sink decides how to honour the hint:
/// terminal-mode flattens to bytes; the TUI applies focus
/// decoration around `Focused` wrappers.
pub enum LayoutHint {
    /// Ok to share a line with adjacent inline-classified
    /// readouts.
    InlineCompact,
    /// Owns its own line(s); sink line-breaks before and
    /// after.
    Block,
    /// Highlighted by the binder's focus state (Push 5);
    /// sink applies an offset / background-tint per its
    /// surface conventions, then defers layout to the
    /// inner hint.
    Focused(Box<LayoutHint>),
}

/// Keyboard events the interactive surface forwards to the
/// binder via [`ReadoutBinder::on_key`]. Push 3's stateless
/// default binder ignores them; Push 5's
/// `TuiReadoutBinder` interprets them.
pub enum BinderKey {
    /// Move focus to the next readout in the current
    /// event slot.
    CycleFocusNext,
    /// Move focus to the previous readout.
    CycleFocusPrev,
    /// Cycle the focused readout's LOD up
    /// (compact → labeled → expanded → compact).
    CycleLodUp,
    /// Cycle the focused readout's LOD down.
    CycleLodDown,
    /// Held-key flip — true on key-down, false on key-up.
    /// While true, every render fires with
    /// `ContentMode::Explanation`.
    OverlayHeld(bool),
}

/// A baked readout body — the artifact the body-grammar
/// parser produces at workload-load time. Cheap to clone
/// (steps are owned, but small) and shared across event
/// fires.
///
/// Construction goes through [`new`](Self::new) /
/// [`from_single`](Self::from_single) /
/// [`from_steps`](Self::from_steps) — the `steps` field is
/// `pub(crate)` so the parser can build directly while
/// preserving room for future invariants (e.g. "first step
/// must be a Render").
#[derive(Debug, Default)]
pub struct BakedBody {
    pub(crate) steps: Vec<RenderStep>,
}

impl BakedBody {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a pre-validated step list. Used by the
    /// body-grammar parser; tests and integration paths
    /// that don't go through the parser construct via this
    /// constructor too.
    pub fn from_steps(steps: Vec<RenderStep>) -> Self {
        Self { steps }
    }

    /// Borrow the step list (read-only). Surfaces that
    /// need to inspect the baked steps (the TUI binder,
    /// snapshot capture) take this view rather than
    /// reaching through the field.
    pub fn steps(&self) -> &[RenderStep] {
        &self.steps
    }

    /// Build from a single registered readout name. Used by
    /// the workload parser's Form-B path (`on_phase_end:
    /// phase_outcome`) where no body grammar is involved.
    pub fn from_single(readout: ReadoutHandle, lod: Lod) -> Self {
        Self::from_steps(vec![RenderStep::Render {
            readout,
            lod,
            layout: LayoutMode::Auto,
            options: ReadoutOptions::new(),
            color: None,
        }])
    }

    /// Walk the step list, calling readouts and writing
    /// literals. The sink mediates layout — the body just
    /// emits steps in order. Inline colour directives
    /// (`@RED` / `[#hex]`) wrap the *next* step in ANSI
    /// on / off bytes; consecutive directives accumulate
    /// (last one wins for the next step).
    pub fn fire(&self, ctx: &dyn ReadoutContext, mode: ContentMode, sink: &mut dyn ReadoutSink) {
        walk_body(self, ctx, mode, sink, &Overrides::default());
    }
}

/// Per-fire overrides applied on top of each step's baked
/// values. Empty for plain [`BakedBody::fire`]; populated
/// by the TUI binder with focus highlighting and per-body
/// LOD overrides from the user's keystrokes.
#[derive(Default, Clone, Copy)]
struct Overrides {
    /// Override the baked LOD on every Render step in this
    /// fire. `None` means "use whatever the step baked".
    lod: Option<Lod>,
    /// Wrap the layout hint in `Focused(...)` so the sink
    /// applies emphasis. `false` means render plain.
    focused: bool,
}

/// One walk of a body's step list. The single source of
/// truth — both the stateless and stateful binders use
/// it. Inline colour directives (`@RED` / `[#hex]`) wrap
/// the next step in ANSI on/off bytes; the per-step
/// `color=` option wins over a pending directive (the
/// option is the more explicit form).
fn walk_body(
    body: &BakedBody,
    ctx: &dyn ReadoutContext,
    mode: ContentMode,
    sink: &mut dyn ReadoutSink,
    overrides: &Overrides,
) {
    let palette = crate::readouts::color::Palette::default();
    let color_enabled = ctx.use_color();
    let mut pending_inline: Option<crate::readouts::color::ColorSpec> = None;

    for step in &body.steps {
        match step {
            RenderStep::ColorDirective(c) => {
                pending_inline = Some(c.clone());
            }
            RenderStep::Literal(s) => {
                if let Some(c) = pending_inline.take() {
                    sink.literal(&c.ansi_open(palette, color_enabled));
                    sink.literal(s);
                    sink.literal(c.ansi_close(color_enabled));
                } else {
                    sink.literal(s);
                }
            }
            RenderStep::Render {
                readout,
                lod,
                layout,
                options,
                color,
            } => {
                let effective_lod = overrides.lod.unwrap_or(*lod);
                let mut hint = layout_hint_for(effective_lod, mode, *layout);
                if overrides.focused {
                    hint = LayoutHint::Focused(Box::new(hint));
                }
                let effective_color = color.clone().or_else(|| pending_inline.take());
                if let Some(c) = effective_color {
                    sink.literal(&c.ansi_open(palette, color_enabled));
                    sink.render(readout.clone(), ctx, effective_lod, mode, options, hint);
                    sink.literal(c.ansi_close(color_enabled));
                } else {
                    sink.render(readout.clone(), ctx, effective_lod, mode, options, hint);
                }
            }
        }
    }
}

/// Per-step layout classification per SRD-63 §7.4.
/// `mode` doesn't affect layout — the overlay shares shape
/// and width with the value per §3.2 — so it isn't an
/// input here. The parameter stays in the signature so a
/// future mode-aware layout (e.g. an "expand on
/// Explanation" rule) doesn't require changing every call
/// site.
pub fn layout_hint_for(lod: Lod, _mode: ContentMode, layout: LayoutMode) -> LayoutHint {
    match layout {
        LayoutMode::Inline => LayoutHint::InlineCompact,
        LayoutMode::Block => LayoutHint::Block,
        // Auto: compact ⇒ inline, labeled / expanded ⇒ block.
        LayoutMode::Auto => match lod {
            Lod::Compact => LayoutHint::InlineCompact,
            Lod::Labeled | Lod::Expanded => LayoutHint::Block,
        },
    }
}

// ── Sink ────────────────────────────────────────────────

/// Layout-aware writer the binder drives. Push 3 ships
/// [`StringSink`] for terminal-mode line emission; Push 5
/// adds a TUI sink that holds `Vec<Span>`.
pub trait ReadoutSink {
    /// Emit a literal run of text. Lives between readout
    /// renders; honours no layout rule on its own — the
    /// surrounding renders do.
    fn literal(&mut self, s: &str);

    /// Render `readout` against `ctx`. The sink applies
    /// `layout` per its surface conventions before / after
    /// invoking `readout.render()`.
    fn render(
        &mut self,
        readout: ReadoutHandle,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        options: &ReadoutOptions,
        layout: LayoutHint,
    );

    /// Force a line break independent of layout.
    fn line_break(&mut self);
}

/// Plain-text line buffer. Concatenates everything into a
/// single `String`; layout hints `Block` / `InlineCompact`
/// resolve to "insert a `\n` before/after Block, share
/// surrounding spaces for Inline." This is what the
/// terminal-mode `\r\x1b[K…` rewriter consumes.
///
/// The sink does not own the eventual stderr write — the
/// caller pulls bytes out via [`StringSink::take`] and
/// emits them.
pub struct StringSink {
    buf: String,
    /// True after a Block-classified readout finished, so
    /// the next non-line-break write inserts a `\n` first.
    pending_break: bool,
    /// True on a fresh sink and after an explicit
    /// `line_break` — the next write doesn't prepend a
    /// space-or-newline.
    fresh_line: bool,
}

impl StringSink {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            pending_break: false,
            fresh_line: true,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: String::with_capacity(cap),
            pending_break: false,
            fresh_line: true,
        }
    }

    /// Consume the sink, returning the rendered string.
    pub fn take(self) -> String {
        self.buf
    }

    /// Borrow the rendered string so far without consuming.
    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

impl Default for StringSink {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadoutSink for StringSink {
    fn literal(&mut self, s: &str) {
        self.flush_pending_break();
        self.buf.push_str(s);
        if !s.is_empty() {
            self.fresh_line = false;
        }
    }

    fn render(
        &mut self,
        readout: ReadoutHandle,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        options: &ReadoutOptions,
        layout: LayoutHint,
    ) {
        // Strip Focused wrappers — the StringSink has no
        // visual focus decoration; it's a flat byte stream.
        // The TUI sink (Push 5) will apply offset / tint
        // before flattening. The `flatten_layout` return
        // type carries only the two outcomes the sink can
        // act on, so the type system removes the
        // never-reached Focused branch.
        match flatten_layout(layout) {
            FlatLayoutHint::Inline => {
                self.flush_pending_break();
                let mut buf = StringBuf::new(&mut self.buf);
                readout.render(ctx, lod, mode, options, &mut buf);
                self.fresh_line = false;
            }
            FlatLayoutHint::Block => {
                // Insert the block-separator newline up front,
                // then call the readout. If it writes nothing
                // (e.g. an error_readout with no errors), roll
                // back the newline AND preserve the prior
                // pending_break / fresh_line state so the next
                // emitter doesn't see a stranded blank line.
                let snapshot_buf_len = self.buf.len();
                let snapshot_pending = self.pending_break;
                let snapshot_fresh = self.fresh_line;
                let inserted_newline = if !self.fresh_line {
                    self.buf.push('\n');
                    true
                } else {
                    false
                };
                let pre_render_len = self.buf.len();
                let mut buf = StringBuf::new(&mut self.buf);
                readout.render(ctx, lod, mode, options, &mut buf);
                if self.buf.len() == pre_render_len {
                    // Zero-byte render — roll back so an empty
                    // readout contributes nothing.
                    if inserted_newline {
                        self.buf.truncate(snapshot_buf_len);
                    }
                    self.pending_break = snapshot_pending;
                    self.fresh_line = snapshot_fresh;
                } else {
                    self.pending_break = true;
                    self.fresh_line = false;
                }
            }
        }
    }

    fn line_break(&mut self) {
        if !self.fresh_line {
            self.buf.push('\n');
        }
        self.pending_break = false;
        self.fresh_line = true;
    }
}

impl StringSink {
    fn flush_pending_break(&mut self) {
        if self.pending_break {
            if !self.fresh_line {
                self.buf.push('\n');
            }
            self.pending_break = false;
            self.fresh_line = true;
        }
    }
}

/// The two outcomes a sink that doesn't decorate focus
/// can act on. `LayoutHint::Focused(inner)` flattens to
/// whatever `inner` resolves to (recursively, in case the
/// builder ever stacks wraps — today it doesn't).
enum FlatLayoutHint {
    Inline,
    Block,
}

fn flatten_layout(layout: LayoutHint) -> FlatLayoutHint {
    match layout {
        LayoutHint::InlineCompact => FlatLayoutHint::Inline,
        LayoutHint::Block => FlatLayoutHint::Block,
        LayoutHint::Focused(inner) => flatten_layout(*inner),
    }
}

// ── Binder trait ────────────────────────────────────────

/// Stateful runtime adapter — drives readouts in response
/// to events, applies any interactive state (Push 5),
/// emits ordered render instructions to the sink.
///
/// The trait is `&mut self` so stateful impls can mutate
/// focus / LOD overrides / overlay-held in-place. That's
/// also why the bound is `Send` (move-across-threads) but
/// not `Sync` (shared-mutable-access): every fire mutates
/// state, so concurrent fires would race. Surfaces that
/// genuinely need cross-thread fire access wrap the binder
/// in a `Mutex` / channel, not a shared reference.
pub trait ReadoutBinder: Send {
    /// Drive every readout bound to `event`. The binder
    /// applies its interactive state, walks the resolved
    /// list, and emits render steps to `sink`.
    fn fire(&mut self, event: EventType, ctx: &dyn ReadoutContext, sink: &mut dyn ReadoutSink);

    /// Forward a keyboard event from the surface. Default
    /// no-op for non-interactive sinks.
    fn on_key(&mut self, _key: BinderKey) {}
}

// ── Default binder ─────────────────────────────────────

/// Stateless default binder. Holds a slot → `Vec<BakedBody>`
/// map; on `fire(event)` walks the matching slot's bodies
/// in declaration order. Push 5 introduces a stateful
/// `TuiReadoutBinder` alongside.
///
/// Behaviour:
/// - No focus state, no LOD overrides, no overlay-held
///   flag.
/// - Each baked body fires with `ContentMode::Value` at
///   the LOD the body itself baked in (per-call
///   `lod=` option, default `Lod::Labeled`).
/// - Multiple bodies bound to the same slot fire in
///   order (the composition rule from SRD-63 §5.5).
pub struct DefaultBinder {
    pub(crate) bindings: HashMap<EventType, Vec<BakedBody>>,
}

impl DefaultBinder {
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
        }
    }

    /// Bind a baked body to an event slot. Multiple calls
    /// with the same event append in order.
    pub fn bind(&mut self, event: EventType, body: BakedBody) {
        self.bindings.entry(event).or_default().push(body);
    }

    /// Replace whatever's at this slot with a single body
    /// — Form A / B's "scalar means single-element list"
    /// path.
    pub fn set(&mut self, event: EventType, body: BakedBody) {
        self.bindings.insert(event, vec![body]);
    }

    /// Number of bodies bound to this slot. 0 means the
    /// event will fire and produce no output — surfaces
    /// expecting a default render need to seed builtins
    /// before calling `fire`.
    pub fn slot_len(&self, event: EventType) -> usize {
        self.bindings.get(&event).map(|v| v.len()).unwrap_or(0)
    }

    /// Drain `other`'s bindings into `self`, appending each
    /// body to its slot. Used by the CLI-override resolver
    /// to combine the workload-+-default-resolved binder
    /// with a CLI-supplied extra body.
    pub fn merge(&mut self, other: DefaultBinder) {
        for (event, bodies) in other.bindings {
            self.bindings.entry(event).or_default().extend(bodies);
        }
    }

    /// Drop every body bound to `event`. Surfaces that
    /// want "no readouts at this slot" call this rather
    /// than binding an empty body.
    pub fn unbind(&mut self, event: EventType) {
        self.bindings.remove(&event);
    }

    /// Remove and return the resolved bodies bound to `event`, leaving the
    /// slot empty. SRD-100 P2 — the executor extracts the resolved
    /// `on_update` template from a freshly-built binder to attach to the
    /// live render handle (`PhaseRenderHandle::bodies`), so the consumer
    /// can fire it with `&self` without holding the `!Sync` binder.
    pub fn take_bodies(&mut self, event: EventType) -> Vec<BakedBody> {
        self.bindings.remove(&event).unwrap_or_default()
    }
}

impl Default for DefaultBinder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadoutBinder for DefaultBinder {
    fn fire(&mut self, event: EventType, ctx: &dyn ReadoutContext, sink: &mut dyn ReadoutSink) {
        if let Some(bodies) = self.bindings.get(&event) {
            for body in bodies {
                body.fire(ctx, ContentMode::Value, sink);
            }
        }
    }
}

/// Build a [`DefaultBinder`] from a workload-level
/// [`nmbrs_workload::model::ReadoutsBindings`] + a fallback
/// table of built-in defaults. The workload's bound bodies
/// replace the matching slot's defaults; unbound slots
/// fall through to whatever defaults the caller seeds.
///
/// Each body string is parsed via
/// [`crate::readouts::parse::bake`]; parse errors fail the
/// build with a descriptive error so the caller surfaces it
/// at workload-load.
///
/// Push 3 keeps this thin — Push 4 layers in the
/// composition / override semantics from SRD-63 §5.4.1
/// (CLI overrides, `+`-prefix append, silent-override
/// warning).
pub fn build_binder_from_workload(
    bindings: &nmbrs_workload::model::ReadoutsBindings,
    defaults: &[(EventType, BakedBody)],
) -> Result<DefaultBinder, String> {
    use super::parse::bake;
    let mut binder = DefaultBinder::new();

    // Seed defaults first so unbound slots get them.
    for (event, body) in defaults {
        let cloned = BakedBody::from_steps(body.steps.iter().map(clone_step).collect());
        validate_body_for_event(&cloned, *event)?;
        binder.bind(*event, cloned);
    }

    // For every event whose slot has at least one workload
    // binding, drop the defaults and replace with the
    // configured bodies.
    let slots: &[(EventType, &[String])] = &[
        (
            EventType::SessionStart,
            bindings.on_session_start.as_slice(),
        ),
        (EventType::SessionEnd, bindings.on_session_end.as_slice()),
        (EventType::PhaseStart, bindings.on_phase_start.as_slice()),
        (EventType::PhaseEnd, bindings.on_phase_end.as_slice()),
        (EventType::EachStart, bindings.on_each_start.as_slice()),
        (EventType::EachEnd, bindings.on_each_end.as_slice()),
        (EventType::ScopeStart, bindings.on_scope_start.as_slice()),
        (EventType::ScopeEnd, bindings.on_scope_end.as_slice()),
        (EventType::Update, bindings.on_update.as_slice()),
    ];
    for (event, bodies) in slots {
        if bodies.is_empty() {
            continue;
        }
        binder.bindings.remove(event);
        for body_str in *bodies {
            let (baked, _warnings) =
                bake(body_str).map_err(|e| format!("readouts.{}: {e}", event.slot_name()))?;
            validate_body_for_event(&baked, *event)?;
            binder.bind(*event, baked);
        }
    }
    Ok(binder)
}

// ── Stateful TUI binder ─────────────────────────────────

/// Stateful runtime adapter for the TUI surface (SRD-63 §7).
///
/// Wraps a [`DefaultBinder`] with three pieces of
/// interactive state:
///
/// - **Focus index** — which baked body in each slot is
///   currently "selected" by the user. The focused body's
///   render emits with `LayoutHint::Focused(...)` so the
///   sink can apply visual emphasis (offset, background
///   tint).
/// - **Per-(slot, body) LOD overrides** — keystrokes
///   cycle the focused body's LOD up / down; the binder
///   applies overrides on top of the body's baked LOD.
/// - **Overlay-held flag** — while true, every render
///   fires with `ContentMode::Explanation` instead of
///   `Value`. Driven by a held key on the surface.
///
/// The binder forwards `fire` through to its inner
/// `DefaultBinder` for actual rendering, then post-
/// processes by applying the focus / LOD / overlay state
/// via a wrapping render walk.
///
/// Construction is the same as a default binder — same
/// slot bindings — but the surface keeps a long-lived
/// instance so state survives across event fires.
pub struct TuiReadoutBinder {
    inner: DefaultBinder,
    /// Focused body index per slot. `None` means no body
    /// is focused (or the slot has no bodies).
    focus: std::collections::HashMap<EventType, Option<usize>>,
    /// LOD override per `(slot, body_index)` — empty
    /// means use the baked LOD.
    lod_overrides: std::collections::HashMap<(EventType, usize), Lod>,
    /// While true every render emits `Explanation`.
    overlay_held: bool,
    /// Last-fired event slot — `on_key` mutates state for
    /// this slot when no explicit slot is referenced.
    last_event: Option<EventType>,
}

impl TuiReadoutBinder {
    pub fn new() -> Self {
        Self {
            inner: DefaultBinder::new(),
            focus: std::collections::HashMap::new(),
            lod_overrides: std::collections::HashMap::new(),
            overlay_held: false,
            last_event: None,
        }
    }

    /// Construct from an existing `DefaultBinder`. Used by
    /// the activity-init plumbing — build the bindings via
    /// the standard layered resolver, then wrap in the
    /// stateful TUI binder for the live surface.
    pub fn from_default(inner: DefaultBinder) -> Self {
        Self {
            inner,
            focus: std::collections::HashMap::new(),
            lod_overrides: std::collections::HashMap::new(),
            overlay_held: false,
            last_event: None,
        }
    }

    pub fn bind(&mut self, event: EventType, body: BakedBody) {
        self.inner.bind(event, body);
    }

    pub fn slot_len(&self, event: EventType) -> usize {
        self.inner.slot_len(event)
    }

    /// Read the current focus index for a slot. Returns
    /// `Some(i)` when a body in `event`'s list is focused;
    /// `None` when the slot has no bodies or focus is
    /// inactive.
    pub fn focus_for(&self, event: EventType) -> Option<usize> {
        self.focus.get(&event).copied().flatten()
    }

    /// Read the active overlay-held flag. Visible to the
    /// surface so it can render an "explanation overlay
    /// active" affordance in chrome.
    pub fn overlay_held(&self) -> bool {
        self.overlay_held
    }

    /// Read the LOD override for a `(slot, body_index)`
    /// pair, if any.
    pub fn lod_override(&self, event: EventType, idx: usize) -> Option<Lod> {
        self.lod_overrides.get(&(event, idx)).copied()
    }
}

impl Default for TuiReadoutBinder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadoutBinder for TuiReadoutBinder {
    fn fire(&mut self, event: EventType, ctx: &dyn ReadoutContext, sink: &mut dyn ReadoutSink) {
        self.last_event = Some(event);
        // Two paths to Explanation mode:
        //   1. Local `overlay_held` toggle — set via
        //      `BinderKey::OverlayHeld(true)`, primarily the
        //      programmatic test path.
        //   2. Process-global hold flag stamped by the TUI
        //      keystroke layer on `?` press, decaying on its
        //      own auto-revert deadline (see `observer::toggle_explain`).
        // Either path activates the overlay — first one to
        // signal wins. The global path is what powers the
        // operator-visible `?` hold-to-explain UX.
        let global_held = crate::observer::is_explain_held();
        let mode = if self.overlay_held || global_held {
            ContentMode::Explanation
        } else {
            ContentMode::Value
        };
        let Some(bodies) = self.inner.bindings.get(&event) else {
            return;
        };
        let focus_idx = self.focus.get(&event).copied().flatten();
        for (i, body) in bodies.iter().enumerate() {
            // LOD override is a per-(slot, body_index)
            // entry stamped by the user's `+`/`-` key
            // cycle. Walk the body's steps replacing each
            // Render's lod with the override (if any).
            let override_lod = self.lod_overrides.get(&(event, i)).copied();
            let focused = focus_idx == Some(i);
            fire_body_with_overrides(body, ctx, mode, sink, override_lod, focused);
        }
    }

    fn on_key(&mut self, key: BinderKey) {
        match key {
            BinderKey::OverlayHeld(v) => {
                self.overlay_held = v;
            }
            BinderKey::CycleFocusNext => {
                self.cycle_focus(1);
            }
            BinderKey::CycleFocusPrev => {
                self.cycle_focus(-1);
            }
            BinderKey::CycleLodUp => {
                self.cycle_focused_lod(1);
            }
            BinderKey::CycleLodDown => {
                self.cycle_focused_lod(-1);
            }
        }
    }
}

impl TuiReadoutBinder {
    /// Move the focus pointer for the most-recently-fired
    /// slot. `delta` is +1 / -1 (cycles wrap).
    fn cycle_focus(&mut self, delta: i32) {
        let Some(slot) = self.last_event else {
            return;
        };
        let Some(bodies) = self.inner.bindings.get(&slot) else {
            return;
        };
        let len = bodies.len();
        if len == 0 {
            return;
        }
        let cur = self.focus.get(&slot).copied().flatten().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(len as i32) as usize;
        self.focus.insert(slot, Some(next));
    }

    /// Cycle the focused body's LOD by `delta` steps
    /// (compact → labeled → expanded → compact). No-op
    /// when no body is focused or the slot has no
    /// bodies.
    fn cycle_focused_lod(&mut self, delta: i32) {
        let Some(slot) = self.last_event else {
            return;
        };
        let Some(focus_opt) = self.focus.get(&slot).copied() else {
            return;
        };
        let Some(idx) = focus_opt else {
            return;
        };

        // Read the body's baked LOD by walking its first
        // Render step (the common case — bodies that
        // start with a Literal don't yield a meaningful
        // base LOD, so we treat them as Labeled).
        let bodies = self.inner.bindings.get(&slot);
        let Some(bodies) = bodies else {
            return;
        };
        let baked_lod = bodies
            .get(idx)
            .and_then(first_render_lod)
            .unwrap_or(Lod::Labeled);
        let cur = self
            .lod_overrides
            .get(&(slot, idx))
            .copied()
            .unwrap_or(baked_lod);
        let next = step_lod(cur, delta);
        self.lod_overrides.insert((slot, idx), next);
    }
}

fn first_render_lod(body: &BakedBody) -> Option<Lod> {
    body.steps.iter().find_map(|step| match step {
        RenderStep::Render { lod, .. } => Some(*lod),
        _ => None,
    })
}

fn step_lod(cur: Lod, delta: i32) -> Lod {
    let order = [Lod::Compact, Lod::Labeled, Lod::Expanded];
    let pos = order.iter().position(|l| *l == cur).unwrap_or(1) as i32;
    let next = (pos + delta).rem_euclid(order.len() as i32) as usize;
    order[next]
}

/// Walk a baked body with per-fire overrides applied. Thin
/// wrapper that builds the override struct and delegates
/// to [`walk_body`] — kept as a named helper so the TUI
/// binder's call site stays readable.
fn fire_body_with_overrides(
    body: &BakedBody,
    ctx: &dyn ReadoutContext,
    mode: ContentMode,
    sink: &mut dyn ReadoutSink,
    override_lod: Option<Lod>,
    focused: bool,
) {
    walk_body(
        body,
        ctx,
        mode,
        sink,
        &Overrides {
            lod: override_lod,
            focused,
        },
    );
}

/// Build a binder bound to a single event slot, applying
/// the SRD-63 §5.4.1 composition / override rules:
///
/// - **No workload binding** → seed with `default`.
/// - **Workload binding with no `+` prefix** → REPLACE
///   `default` entirely with the workload bodies.
///   (Rule 2.)
/// - **Workload binding with one or more `+`-prefixed
///   entries** → keep `default` and APPEND the prefixed
///   bodies after it. Plain entries in the same list
///   still REPLACE the default; mixing `+` and plain in
///   the same list means "replace with this list (which
///   happens to also extend somewhere)". (Rule 3.)
///
/// Cheap enough to call once per phase or per refresh
/// tick — Push 3 wires it at activity-init for the two
/// slots `on_update` and `on_phase_end`.
pub fn build_event_binder(
    bindings: &nmbrs_workload::model::ReadoutsBindings,
    event: EventType,
    default: BakedBody,
) -> Result<DefaultBinder, String> {
    build_event_binder_with_cli(bindings, event, default, None)
}

/// Same as [`build_event_binder`], with a CLI `--readout`
/// override layered on top per SRD-63 §8 / Push 8. The
/// override only applies to the `Update` slot (the only
/// slot the single `--readout` flag targets); other slots
/// resolve through the workload + default path. Push 9+
/// could grow per-event override flags
/// (`--readout-on-each=…`) if demand arises.
///
/// Resolution semantics:
/// - `cli_override = None`: identical to
///   [`build_event_binder`] — workload-then-default.
/// - `cli_override = Some(body)` and `event == Update`:
///   the body REPLACES whatever the workload + default
///   path would have bound. A non-default workload
///   binding being silently replaced emits a warning per
///   SRD-63 §5.4.1 Rule 2's safety net.
/// - `cli_override = Some(body)` and `event != Update`:
///   the override is ignored for this slot — the single
///   `--readout` flag's contract is on_update only.
pub fn build_event_binder_with_cli(
    bindings: &nmbrs_workload::model::ReadoutsBindings,
    event: EventType,
    default: BakedBody,
    cli_override: Option<&str>,
) -> Result<DefaultBinder, String> {
    use super::parse::bake;

    if let Some(body_str) = cli_override
        && event == EventType::Update
    {
        // SRD-63 §5.4.1 Rule 2 safety net: warn loudly
        // when a non-default workload binding is being
        // silently replaced by the CLI flag.
        let workload_bodies = bindings.get(event.slot_name());
        if !workload_bodies.is_empty() {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "readouts: --readout override [{body}] replaces workload binding {workload:?} \
                 for slot {slot}. Use a `+` prefix on the override (e.g. `--readout=+x`) \
                 if you intended to extend rather than replace.",
                body = body_str,
                workload = workload_bodies,
                slot = event.slot_name(),
            );
        }
        let mut binder = DefaultBinder::new();
        let stripped = body_str.trim_start().strip_prefix('+').unwrap_or(body_str);
        let plus_prefix = body_str.trim_start().starts_with('+');
        if plus_prefix {
            // `+` form on the CLI: keep workload + default
            // path's bodies and append the override body.
            let inner = build_event_binder(bindings, event, default)?;
            binder.merge(inner);
            let (baked, _) = bake(stripped).map_err(|e| format!("readouts: --readout: {e}"))?;
            validate_body_for_event(&baked, event)?;
            binder.bind(event, baked);
        } else {
            let (baked, _) = bake(stripped).map_err(|e| format!("readouts: --readout: {e}"))?;
            validate_body_for_event(&baked, event)?;
            binder.bind(event, baked);
        }
        return Ok(binder);
    }

    build_event_binder_inner(bindings, event, default)
}

fn build_event_binder_inner(
    bindings: &nmbrs_workload::model::ReadoutsBindings,
    event: EventType,
    default: BakedBody,
) -> Result<DefaultBinder, String> {
    use super::parse::bake;
    let mut binder = DefaultBinder::new();
    let bodies = bindings.get(event.slot_name());
    if bodies.is_empty() {
        validate_body_for_event(&default, event)?;
        binder.bind(event, default);
        return Ok(binder);
    }

    let any_plain = bodies.iter().any(|b| !b.trim_start().starts_with('+'));
    let any_appended = bodies.iter().any(|b| b.trim_start().starts_with('+'));

    // Pure-append mode: every entry is `+`-prefixed →
    // keep the default and append. Mixed mode (plain +
    // append) treats the whole list as a replacement
    // that includes the appended entries inline.
    if !any_plain && any_appended {
        validate_body_for_event(&default, event)?;
        binder.bind(event, default);
    }

    for body_str in bodies {
        let stripped = body_str.trim_start().strip_prefix('+').unwrap_or(body_str);
        let (baked, _warnings) =
            bake(stripped).map_err(|e| format!("readouts.{}: {e}", event.slot_name()))?;
        validate_body_for_event(&baked, event)?;
        binder.bind(event, baked);
    }
    Ok(binder)
}

fn clone_step(step: &RenderStep) -> RenderStep {
    match step {
        RenderStep::Literal(s) => RenderStep::Literal(s.clone()),
        RenderStep::Render {
            readout,
            lod,
            layout,
            options,
            color,
        } => RenderStep::Render {
            readout: readout.clone(),
            lod: *lod,
            layout: *layout,
            options: options.clone(),
            color: color.clone(),
        },
        RenderStep::ColorDirective(c) => RenderStep::ColorDirective(c.clone()),
    }
}

/// Bake-time validation: every Render step in `body` must
/// accept the firing slot's subject kind. The binder calls
/// this before binding, so a workload mistakenly binding
/// `phase_status` to `on_session_end` errors at workload-
/// load instead of rendering silent zeros at run time.
///
/// Per `feedback_never_ignore_silently` — every input must
/// be acted on or rejected, never discarded.
pub fn validate_body_for_event(body: &BakedBody, event: EventType) -> Result<(), String> {
    let slot_kind = event.subject_kind();
    for step in &body.steps {
        if let RenderStep::Render { readout, .. } = step {
            let accepted = readout.accepts();
            if !accepted.contains(&slot_kind) {
                return Err(format!(
                    "readouts.{slot}: readout '{name}' does not accept \
                     subject kind {slot_kind:?} (accepts {accepted:?})",
                    slot = event.slot_name(),
                    name = readout.name(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::Registry;

    // ── Bake-time subject-kind validation ─────────────────

    #[test]
    fn validate_rejects_phase_readout_at_session_slot() {
        // phase_status accepts only Phase. Binding it at
        // `on_session_end` (Session-kind subject) should
        // error at workload-load — not silently render zeros.
        let bindings = nmbrs_workload::model::ReadoutsBindings {
            on_session_end: vec!["phase_status".to_string()],
            ..Default::default()
        };
        let res = build_event_binder(&bindings, EventType::SessionEnd, BakedBody::new());
        let err = match res {
            Ok(_) => panic!("expected validation error"),
            Err(e) => e,
        };
        assert!(err.contains("phase_status"), "{err}");
        assert!(err.contains("does not accept"), "{err}");
        assert!(err.contains("Session"), "{err}");
    }

    #[test]
    fn validate_accepts_session_summary_at_session_slot() {
        let bindings = nmbrs_workload::model::ReadoutsBindings {
            on_session_end: vec!["session_summary".to_string()],
            ..Default::default()
        };
        assert!(
            build_event_binder(&bindings, EventType::SessionEnd, BakedBody::new(),).is_ok(),
            "session_summary at on_session_end should be valid"
        );
    }

    #[test]
    fn validate_accepts_trace_at_every_slot() {
        // trace declares it accepts all subject kinds —
        // useful as a wildcard diagnostic.
        // One case: an event kind paired with a setter that binds `trace`
        // at the matching slot.
        type SlotCase = (EventType, fn(&mut nmbrs_workload::model::ReadoutsBindings));
        let cases: &[SlotCase] = &[
            (EventType::SessionEnd, |b| {
                b.on_session_end = vec!["trace".into()]
            }),
            (EventType::PhaseEnd, |b| {
                b.on_phase_end = vec!["trace".into()]
            }),
            (EventType::EachEnd, |b| b.on_each_end = vec!["trace".into()]),
            (EventType::ScopeEnd, |b| {
                b.on_scope_end = vec!["trace".into()]
            }),
        ];
        for (event, set_slot) in cases {
            let mut bindings = nmbrs_workload::model::ReadoutsBindings::default();
            set_slot(&mut bindings);
            assert!(
                build_event_binder(&bindings, *event, BakedBody::new()).is_ok(),
                "trace at {event:?} should validate"
            );
        }
    }

    /// Sink a Default-bound `phase_outcome` against a tiny
    /// hand-rolled context, assert the rendered string is
    /// non-empty and contains the expected ✓.
    #[test]
    fn default_binder_fires_phase_outcome_at_phase_end() {
        struct Ctx;
        impl ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "setup"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                Some((1, 2))
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                3
            }
            fn cycles_total(&self) -> u64 {
                3
            }
            fn ops_ok(&self) -> u64 {
                3
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.01
            }
            fn consumed(&self) -> u64 {
                3
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::PhaseEnd
            }
        }
        let mut binder = DefaultBinder::new();
        let phase_outcome = Registry::lookup("phase_outcome").unwrap();
        binder.set(
            EventType::PhaseEnd,
            BakedBody::from_single(phase_outcome, Lod::Labeled),
        );

        let mut sink = StringSink::new();
        binder.fire(EventType::PhaseEnd, &Ctx, &mut sink);

        let out = sink.take();
        assert!(out.contains("✓"), "phase_outcome's ✓ missing: {out}");
        assert!(out.contains("[setup]"), "phase name missing: {out}");
        // Single-placement rule: the margin owns [n/N]; the body must
        // NOT carry a seq prefix.
        assert!(!out.contains("[1/2]"), "seq must not appear in body: {out}");
    }

    #[test]
    fn default_binder_dispatches_only_to_matching_event() {
        struct Ctx;
        impl ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "x"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                None
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                0
            }
            fn cycles_total(&self) -> u64 {
                0
            }
            fn ops_ok(&self) -> u64 {
                0
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.0
            }
            fn consumed(&self) -> u64 {
                0
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::PhaseEnd
            }
        }
        let mut binder = DefaultBinder::new();
        let phase_outcome = Registry::lookup("phase_outcome").unwrap();
        binder.set(
            EventType::PhaseEnd,
            BakedBody::from_single(phase_outcome, Lod::Labeled),
        );

        // Fire the wrong event — sink should stay empty.
        let mut sink = StringSink::new();
        binder.fire(EventType::Update, &Ctx, &mut sink);
        assert_eq!(sink.take(), "");
    }

    #[test]
    fn string_sink_block_inserts_newlines() {
        let mut sink = StringSink::new();
        sink.literal("a");
        // simulate a block-render with a fake step:
        // we can't easily call a Readout here, so test
        // line-break behaviour directly.
        sink.line_break();
        sink.literal("b");
        sink.line_break();
        // Multiple consecutive line_breaks are idempotent.
        sink.line_break();
        sink.literal("c");
        assert_eq!(sink.take(), "a\nb\nc");
    }

    /// Regression: a Block-classified readout that writes nothing
    /// must contribute nothing — no stranded blank line. The
    /// failure shape was `error_readout` (empty errors) emitting
    /// its block-separator newline before discovering it had no
    /// content, leaving the prior block-separator pending state +
    /// the new newline = visible blank row between phase outcomes.
    #[test]
    fn empty_block_render_contributes_no_blank_line() {
        use crate::lifecycle::SubjectKind;
        use crate::readouts::buf::ReadoutBuf;
        struct EmptyReadout;
        impl Readout for EmptyReadout {
            fn name(&self) -> &'static str {
                "empty"
            }
            fn accepts(&self) -> &'static [SubjectKind] {
                &[SubjectKind::Phase]
            }
            fn render(
                &self,
                _ctx: &dyn ReadoutContext,
                _lod: Lod,
                _mode: ContentMode,
                _opts: &ReadoutOptions,
                _out: &mut dyn ReadoutBuf,
            ) -> usize {
                0
            }
        }
        struct Filler;
        impl Readout for Filler {
            fn name(&self) -> &'static str {
                "filler"
            }
            fn accepts(&self) -> &'static [SubjectKind] {
                &[SubjectKind::Phase]
            }
            fn render(
                &self,
                _ctx: &dyn ReadoutContext,
                _lod: Lod,
                _mode: ContentMode,
                _opts: &ReadoutOptions,
                out: &mut dyn ReadoutBuf,
            ) -> usize {
                let _ = out.write_str("filler-text");
                "filler-text".len()
            }
        }
        struct Ctx;
        impl ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "x"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                None
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                0
            }
            fn cycles_total(&self) -> u64 {
                0
            }
            fn ops_ok(&self) -> u64 {
                0
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                0
            }
            fn elapsed_secs(&self) -> f64 {
                0.0
            }
            fn consumed(&self) -> u64 {
                0
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::PhaseEnd
            }
        }
        let mut sink = StringSink::new();
        let opts = ReadoutOptions::new();
        let h_filler: ReadoutHandle = std::sync::Arc::new(Filler);
        let h_empty: ReadoutHandle = std::sync::Arc::new(EmptyReadout);

        // Block A: writes content.
        sink.render(
            h_filler.clone(),
            &Ctx,
            Lod::Labeled,
            ContentMode::Value,
            &opts,
            LayoutHint::Block,
        );
        // Block B: empty render. Must NOT introduce a blank line.
        sink.render(
            h_empty,
            &Ctx,
            Lod::Labeled,
            ContentMode::Value,
            &opts,
            LayoutHint::Block,
        );
        // Block C: writes content.
        sink.render(
            h_filler,
            &Ctx,
            Lod::Labeled,
            ContentMode::Value,
            &opts,
            LayoutHint::Block,
        );

        // Expected: filler + \n + filler — single block separator,
        // not filler + \n + \n + filler (which would be the stranded-
        // blank-line failure).
        assert_eq!(sink.take(), "filler-text\nfiller-text");
    }

    #[test]
    fn layout_hint_auto_picks_inline_for_compact() {
        assert!(matches!(
            layout_hint_for(Lod::Compact, ContentMode::Value, LayoutMode::Auto),
            LayoutHint::InlineCompact
        ));
        assert!(matches!(
            layout_hint_for(Lod::Labeled, ContentMode::Value, LayoutMode::Auto),
            LayoutHint::Block
        ));
        assert!(matches!(
            layout_hint_for(Lod::Expanded, ContentMode::Value, LayoutMode::Auto),
            LayoutHint::Block
        ));
    }

    #[test]
    fn layout_hint_inline_overrides_lod() {
        // Force inline at expanded LOD (workload author's
        // explicit choice; sink's job to detect overflow).
        assert!(matches!(
            layout_hint_for(Lod::Expanded, ContentMode::Value, LayoutMode::Inline),
            LayoutHint::InlineCompact
        ));
    }

    #[test]
    fn layout_hint_block_overrides_lod() {
        // Force block at compact LOD (workload author wants
        // emphasis on a normally-inline readout).
        assert!(matches!(
            layout_hint_for(Lod::Compact, ContentMode::Value, LayoutMode::Block),
            LayoutHint::Block
        ));
    }

    // ── Composition / override resolver ──────────────────

    fn empty_bindings() -> nmbrs_workload::model::ReadoutsBindings {
        nmbrs_workload::model::ReadoutsBindings::default()
    }

    fn default_phase_outcome() -> BakedBody {
        BakedBody::from_single(Registry::lookup("phase_outcome").unwrap(), Lod::Labeled)
    }

    #[test]
    fn no_workload_binding_uses_default() {
        // Slot is empty → builder uses the supplied default.
        let bindings = empty_bindings();
        let binder =
            build_event_binder(&bindings, EventType::PhaseEnd, default_phase_outcome()).unwrap();
        assert_eq!(binder.slot_len(EventType::PhaseEnd), 1);
    }

    #[test]
    fn plain_workload_binding_replaces_default() {
        // Rule 2: a plain (non-prefixed) workload binding
        // REPLACES the default fully.
        let mut bindings = empty_bindings();
        bindings.on_phase_end = vec!["trace".to_string()];
        let binder =
            build_event_binder(&bindings, EventType::PhaseEnd, default_phase_outcome()).unwrap();
        // One body bound — the workload's, default dropped.
        assert_eq!(binder.slot_len(EventType::PhaseEnd), 1);
    }

    #[test]
    fn plus_prefix_workload_binding_appends_to_default() {
        // Rule 3: every entry `+`-prefixed → KEEP default
        // and append.
        let mut bindings = empty_bindings();
        bindings.on_phase_end = vec!["+trace".to_string()];
        let binder =
            build_event_binder(&bindings, EventType::PhaseEnd, default_phase_outcome()).unwrap();
        // Two bodies: the default + the appended trace.
        assert_eq!(binder.slot_len(EventType::PhaseEnd), 2);
    }

    #[test]
    fn multiple_plus_prefix_appends_in_order() {
        let mut bindings = empty_bindings();
        bindings.on_phase_end = vec!["+trace".to_string(), "+trace".to_string()];
        let binder =
            build_event_binder(&bindings, EventType::PhaseEnd, default_phase_outcome()).unwrap();
        // default + 2 appended.
        assert_eq!(binder.slot_len(EventType::PhaseEnd), 3);
    }

    #[test]
    fn cli_override_replaces_workload_binding_at_update() {
        let mut bindings = empty_bindings();
        bindings.on_update = vec!["phase_status".to_string()];
        let binder = build_event_binder_with_cli(
            &bindings,
            EventType::Update,
            default_phase_outcome(),
            Some("trace"),
        )
        .unwrap();
        // CLI override → exactly one body (the override),
        // workload's binding dropped.
        assert_eq!(binder.slot_len(EventType::Update), 1);
    }

    #[test]
    fn cli_override_plus_prefix_appends_to_workload_resolved() {
        let mut bindings = empty_bindings();
        bindings.on_update = vec!["phase_status".to_string()];
        let binder = build_event_binder_with_cli(
            &bindings,
            EventType::Update,
            default_phase_outcome(),
            Some("+trace"),
        )
        .unwrap();
        // workload phase_status + appended trace = 2 bodies.
        assert_eq!(binder.slot_len(EventType::Update), 2);
    }

    #[test]
    fn cli_override_only_applies_to_update_slot() {
        let bindings = empty_bindings();
        let binder = build_event_binder_with_cli(
            &bindings,
            EventType::PhaseEnd,
            default_phase_outcome(),
            Some("trace"),
        )
        .unwrap();
        // PhaseEnd ignores --readout — falls back to default.
        assert_eq!(binder.slot_len(EventType::PhaseEnd), 1);
        // The default body is phase_outcome, not trace; verify
        // by re-firing and checking output starts with ✓.
        struct Ctx;
        impl ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "x"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                None
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                0
            }
            fn cycles_total(&self) -> u64 {
                0
            }
            fn ops_ok(&self) -> u64 {
                0
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.0
            }
            fn consumed(&self) -> u64 {
                0
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::PhaseEnd
            }
        }
        let mut binder_local = binder;
        let mut sink = StringSink::new();
        binder_local.fire(EventType::PhaseEnd, &Ctx, &mut sink);
        let out = sink.take();
        assert!(
            out.contains("✓"),
            "default phase_outcome body should fire: {out}"
        );
    }

    #[test]
    fn mixed_plain_and_plus_treats_whole_list_as_replacement() {
        // Plain entry present in the list → REPLACE mode
        // applies to every entry (the `+` prefix becomes
        // editorial only, the binding drops the default).
        let mut bindings = empty_bindings();
        bindings.on_phase_end = vec!["trace".to_string(), "+trace".to_string()];
        let binder =
            build_event_binder(&bindings, EventType::PhaseEnd, default_phase_outcome()).unwrap();
        // Two bodies (the workload's two), no default.
        assert_eq!(binder.slot_len(EventType::PhaseEnd), 2);
    }

    // ── TuiReadoutBinder ─────────────────────────────────

    fn make_tui_binder_with_two_bodies() -> TuiReadoutBinder {
        let mut binder = TuiReadoutBinder::new();
        let phase_outcome = Registry::lookup("phase_outcome").unwrap();
        let trace = Registry::lookup("trace").unwrap();
        binder.bind(
            EventType::PhaseEnd,
            BakedBody::from_single(phase_outcome, Lod::Labeled),
        );
        binder.bind(
            EventType::PhaseEnd,
            BakedBody::from_single(trace, Lod::Labeled),
        );
        binder
    }

    #[test]
    fn tui_binder_overlay_held_toggles_mode() {
        let mut binder = TuiReadoutBinder::new();
        assert!(!binder.overlay_held());
        binder.on_key(BinderKey::OverlayHeld(true));
        assert!(binder.overlay_held());
        binder.on_key(BinderKey::OverlayHeld(false));
        assert!(!binder.overlay_held());
    }

    /// The process-global `?`-toggle flag is an OR-with-local
    /// path into Explanation mode: when on, every `fire()`
    /// dispatches with `ContentMode::Explanation` regardless
    /// of the binder's local `overlay_held` toggle. This is
    /// what powers the TUI's `?`-to-explain UX without
    /// threading a channel into each binder.
    ///
    /// Toggle semantics (not hold): one press → on; second
    /// press → off. Tests below cover both transitions plus
    /// the auto-repeat debounce. We avoid the 10 s auto-revert
    /// by manually toggling off rather than waiting for the
    /// deadline.
    ///
    /// Tests in this module that exercise the process-global
    /// `EXPLAIN_HELD_UNTIL_NS` / `EXPLAIN_LAST_PRESS_NS` atomics
    /// in observer.rs serialize against each other via the
    /// mutex below. Production code is correct (debounce +
    /// auto-revert is the intended behavior); the issue is that
    /// cargo's default parallel test execution lets two tests
    /// race on shared static state.
    #[test]
    fn global_explain_toggle_flips_explanation_mode() {
        let _guard = EXPLAIN_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Force baseline off by toggling until off. (A previous
        // test may have left it on; the debounce + 10 s deadline
        // mean we can't just wait for decay in a unit test.)
        // Two toggles in sequence: first might be no-op due to
        // debounce, second always applies after a short sleep.
        std::thread::sleep(std::time::Duration::from_millis(300));
        if crate::observer::is_explain_held() {
            crate::observer::toggle_explain();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        assert!(
            !crate::observer::is_explain_held(),
            "baseline: explain flag MUST be off"
        );
        // First press → on.
        crate::observer::toggle_explain();
        assert!(
            crate::observer::is_explain_held(),
            "first press MUST turn the flag on"
        );
        // Second press past the debounce window → off.
        std::thread::sleep(std::time::Duration::from_millis(300));
        crate::observer::toggle_explain();
        assert!(
            !crate::observer::is_explain_held(),
            "second press MUST turn the flag off"
        );
    }

    static EXPLAIN_GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Auto-repeat debounce: a second `toggle_explain` call
    /// within 250 ms of the first is swallowed. The state
    /// stays where the first press left it.
    #[test]
    fn global_explain_toggle_debounces_auto_repeat() {
        let _guard = EXPLAIN_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Force baseline off, with appropriate spacing.
        std::thread::sleep(std::time::Duration::from_millis(300));
        if crate::observer::is_explain_held() {
            crate::observer::toggle_explain();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        assert!(!crate::observer::is_explain_held(), "baseline: off");
        // First press → on.
        crate::observer::toggle_explain();
        assert!(crate::observer::is_explain_held());
        // Immediately repeated → should be swallowed (debounce).
        crate::observer::toggle_explain();
        assert!(
            crate::observer::is_explain_held(),
            "rapid second press MUST be debounced, leaving state on"
        );
        // Clean up: wait for debounce window, toggle off.
        std::thread::sleep(std::time::Duration::from_millis(300));
        crate::observer::toggle_explain();
    }

    #[test]
    fn tui_binder_focus_cycles_through_slot() {
        struct Ctx;
        impl ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "x"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                None
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                0
            }
            fn cycles_total(&self) -> u64 {
                0
            }
            fn ops_ok(&self) -> u64 {
                0
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.0
            }
            fn consumed(&self) -> u64 {
                0
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::PhaseEnd
            }
        }
        let mut binder = make_tui_binder_with_two_bodies();
        // First fire to set last_event.
        let mut sink = StringSink::new();
        binder.fire(EventType::PhaseEnd, &Ctx, &mut sink);
        // No focus stamped yet.
        assert_eq!(binder.focus_for(EventType::PhaseEnd), None);

        binder.on_key(BinderKey::CycleFocusNext);
        // Focus is now at body 1 — we treated `None`
        // as "before-first" so the next-cycle starts from 0
        // and adds delta=1 → 1.
        assert_eq!(binder.focus_for(EventType::PhaseEnd), Some(1));

        binder.on_key(BinderKey::CycleFocusNext);
        // Wraps back to 0 (two bodies).
        assert_eq!(binder.focus_for(EventType::PhaseEnd), Some(0));

        binder.on_key(BinderKey::CycleFocusPrev);
        // Wraps the other way to 1.
        assert_eq!(binder.focus_for(EventType::PhaseEnd), Some(1));
    }

    #[test]
    fn tui_binder_lod_cycle_stamps_override() {
        struct Ctx;
        impl ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "x"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                None
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                0
            }
            fn cycles_total(&self) -> u64 {
                0
            }
            fn ops_ok(&self) -> u64 {
                0
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.0
            }
            fn consumed(&self) -> u64 {
                0
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::PhaseEnd
            }
        }
        let mut binder = make_tui_binder_with_two_bodies();
        let mut sink = StringSink::new();
        binder.fire(EventType::PhaseEnd, &Ctx, &mut sink);
        binder.on_key(BinderKey::CycleFocusNext); // focus → body 1

        // Initial baked LOD is Labeled. Cycle up → Expanded.
        binder.on_key(BinderKey::CycleLodUp);
        assert_eq!(
            binder.lod_override(EventType::PhaseEnd, 1),
            Some(Lod::Expanded)
        );

        // Cycle up again → wraps to Compact.
        binder.on_key(BinderKey::CycleLodUp);
        assert_eq!(
            binder.lod_override(EventType::PhaseEnd, 1),
            Some(Lod::Compact)
        );

        // Cycle down → back to Expanded (wrap).
        binder.on_key(BinderKey::CycleLodDown);
        assert_eq!(
            binder.lod_override(EventType::PhaseEnd, 1),
            Some(Lod::Expanded)
        );
    }

    #[test]
    fn step_lod_cycles_three_levels() {
        assert_eq!(step_lod(Lod::Compact, 1), Lod::Labeled);
        assert_eq!(step_lod(Lod::Labeled, 1), Lod::Expanded);
        assert_eq!(step_lod(Lod::Expanded, 1), Lod::Compact);
        assert_eq!(step_lod(Lod::Compact, -1), Lod::Expanded);
    }
}
