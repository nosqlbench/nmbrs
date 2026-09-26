// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! A node of nmbrs's scope tree: a kernel on the fiber engine, beside the
//! interpreter program scope synthesis reads.
//!
//! polydat splits a scope in two (native_scope_trees.md §2): the analysis
//! program — which inputs are coordinates, what a child re-emits from its
//! parent, checkpoint identity, the describe views — and the kernel that
//! runs, on any engine. A [`ScopeKernel`] keeps both. Synthesis reads
//! [`ScopeKernel::program`]; everything that evaluates goes through the
//! engine-neutral `Kernel` the type dereferences to, and children are
//! bound under it with `bind_under` / `instantiate_under`.
//!
//! The running kernel is on [`crate::fiber_engine::fiber_engine`] when the
//! engine's image lists the program's inputs and outputs in the same
//! order (so an index resolved on the program drives it), and on the
//! interpreter otherwise.

use std::sync::Arc;

use polydat::Kernel;
use polydat::ast::Value;
use polydat::kernel::interp::{KernelLookup, Lookup};
use polydat::kernel::{KernelProgram, PolydatKernel, PolydatProgram};

use crate::fiber_engine::OpTemplateModule;

/// A scope kernel and the interpreter program it stands for.
pub struct ScopeKernel {
    kernel: Box<dyn Kernel>,
    /// The analysis program; positions agree with `kernel`'s.
    program: Arc<PolydatProgram>,
    /// The program `kernel` runs, which an iteration of this scope binds.
    image: Arc<dyn KernelProgram>,
    /// The module a source-built scope finalized to: another instance
    /// carries its write-throughs.
    module: Option<Arc<OpTemplateModule>>,
}

impl std::fmt::Debug for ScopeKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeKernel")
            .field("engine", &self.kernel.engine())
            .field("outputs", &self.program.output_names())
            .finish()
    }
}

impl std::ops::Deref for ScopeKernel {
    type Target = dyn Kernel;
    fn deref(&self) -> &Self::Target {
        self.kernel.as_ref()
    }
}

impl std::ops::DerefMut for ScopeKernel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.kernel.as_mut()
    }
}

impl From<PolydatKernel> for ScopeKernel {
    /// An interpreter kernel as a scope: a kernel standing outside the
    /// tree's construction (a test fixture, a probe) that a scope is
    /// bound under.
    fn from(kernel: PolydatKernel) -> Self {
        let program = kernel.program().clone();
        Self {
            image: program.clone(),
            program,
            kernel: Box::new(kernel),
            module: None,
        }
    }
}

impl ScopeKernel {
    /// A root scope compiled from source: `interpreter` is the compiled
    /// kernel, and `image` the fiber engine's program for the same source
    /// and options, when it can stand in for `interpreter`'s program
    /// ([`crate::fiber_engine::source_image`]).
    pub fn root(interpreter: PolydatKernel, image: Option<Arc<dyn KernelProgram>>) -> Self {
        match image {
            Some(image) => Self {
                program: interpreter.program().clone(),
                kernel: image.clone().create_kernel(),
                image,
                module: None,
            },
            None => Self::from(interpreter),
        }
    }

    /// A root scope compiled from `source` with default options
    /// ([`crate::bindings::compile_scope_kernel`]).
    pub fn compile(source: &str) -> Result<Self, String> {
        crate::bindings::compile_scope_kernel(source, &Default::default())
    }

    /// A child scope built from source matter under `parent` — the
    /// any-engine form of `build_subscope` (native_scope_trees.md §5):
    /// the matter finalizes to a module, which is instantiated under
    /// `parent`. Strict mode refuses a const that silently fell through
    /// to an outer value, as `build_subscope` does.
    pub fn build_under(parent: &dyn Kernel, matter: SourceMatter) -> Result<Self, String> {
        use polydat::kernel::subcontext::{
            ContractViolation, RootMarker, SourceContext, SubcontextBuilder,
        };
        let SourceMatter {
            label,
            body,
            inherited_outputs,
            options,
            result_bindings,
        } = matter;
        let strict = options.strict;
        let mut builder: SubcontextBuilder<RootMarker> = SubcontextBuilder::under(parent);
        builder
            .context(SourceContext::new(label.clone()))
            .mark_inherited_outputs(inherited_outputs)
            .with_compile_options(options)
            .body(body);
        if let Some(src) = result_bindings {
            builder
                .add_result_bindings(&src)
                .map_err(|e| e.to_string())?;
        }
        let module = Arc::new(builder.finalize().map_err(|e| e.to_string())?);
        let program = module.program().clone();
        let image = crate::fiber_engine::module_image(&module, &label)
            .unwrap_or_else(|| program.clone() as Arc<dyn KernelProgram>);
        let kernel = module
            .instantiate_under(parent, image.engine(), &[])
            .map_err(|e| e.to_string())?;
        let scope = Self {
            kernel,
            program,
            image,
            module: Some(module),
        };
        if strict {
            let fell_through = scope.silent_fall_throughs();
            if !fell_through.is_empty() {
                return Err(ContractViolation::StrictNonePropagation {
                    bindings: fell_through,
                    site: SourceContext::new(label),
                }
                .to_string());
            }
        }
        Ok(scope)
    }

    /// [`Self::build_under`] a parent scope, then write each of the
    /// parent's input values into the child's input of the same name —
    /// how every synthesized scope (phase, for_each, do-loop, op
    /// template) is built. A value the child's input refuses is an error.
    pub fn synthesize_under(parent: &ScopeKernel, matter: SourceMatter) -> Result<Self, String> {
        let mut child = Self::build_under(parent.kernel(), matter)?;
        polydat::kernel::propagate_inputs(parent.kernel(), child.kernel_mut())
            .map_err(|e| e.to_string())?;
        Ok(child)
    }

    /// Another instance of this scope's program under `parent`, with
    /// `bindings` written into its inputs first — the any-engine form of
    /// `for_iteration(canonical, parent, bindings)`, and of
    /// `build_subscope` with program matter.
    pub fn bind_under(
        &self,
        parent: &dyn Kernel,
        bindings: &[(String, Value)],
    ) -> Result<Self, String> {
        let kernel = match &self.module {
            Some(module) => module.instantiate_under(parent, self.image.engine(), bindings),
            None => polydat::kernel::bind_under(parent, self.image.clone(), bindings),
        }
        .map_err(|e| e.to_string())?;
        Ok(self.with_kernel(kernel))
    }

    /// A copy of this scope with its state — inputs, outputs, cells
    /// shared (native_scope_trees.md §4): a fiber's starting kernel, an
    /// activation scope, a probe that leaves this one undisturbed.
    pub fn fork(&self) -> Self {
        self.with_kernel(self.kernel.fork())
    }

    fn with_kernel(&self, kernel: Box<dyn Kernel>) -> Self {
        Self {
            kernel,
            program: self.program.clone(),
            image: self.image.clone(),
            module: self.module.clone(),
        }
    }

    /// The interpreter program this scope stands for: what synthesis and
    /// checkpoint identity read.
    pub fn program(&self) -> &Arc<PolydatProgram> {
        &self.program
    }

    /// The program the running kernel is on.
    pub fn image(&self) -> &Arc<dyn KernelProgram> {
        &self.image
    }

    /// The module a source-built scope finalized to, from which each
    /// fiber instantiates its own kernel with the module's write-throughs.
    pub fn module(&self) -> Option<&Arc<OpTemplateModule>> {
        self.module.as_ref()
    }

    /// The running kernel.
    pub fn kernel(&self) -> &dyn Kernel {
        self.kernel.as_ref()
    }

    /// The running kernel, for a holder of a bare `dyn Kernel` — an
    /// adapter's canonical kernel. Its `program_id` is this scope's.
    pub fn into_kernel(self) -> Box<dyn Kernel> {
        self.kernel
    }

    /// The running kernel, mutably.
    pub fn kernel_mut(&mut self) -> &mut dyn Kernel {
        self.kernel.as_mut()
    }

    /// A name's value in this scope without evaluating anything: a
    /// const's value, an input, a value the build folded. A computed
    /// output is not a scope value, before or after a pull, on any
    /// engine; [`Self::pull_value`] evaluates one.
    pub fn lookup(&self, name: &str) -> Option<Value> {
        KernelLookup::new(self.kernel.as_ref()).lookup(name)
    }

    /// A name's value, evaluating a computed output on a fork so this
    /// scope's state is undisturbed: [`Self::lookup`] first, then a pull.
    pub fn pull_value(&self, name: &str) -> Option<Value> {
        self.lookup(name).or_else(|| {
            let idx = self.program.output_index(name)?;
            Some(self.kernel.fork().pull_at(idx))
        })
    }

    /// The values this scope's inputs hold, by name, for writing into
    /// other kernels: every input with a value except a const's slot,
    /// which only initialization writes (each kernel the values go into
    /// initializes its own consts from them).
    pub fn scope_values(&self) -> Vec<(String, Value)> {
        self.program
            .input_names()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| self.program.input_kind(*i) != Some(polydat::kernel::InputKind::Const))
            .filter_map(|(i, name)| match self.kernel.input_value_at(i) {
                Some(Value::None) | None => None,
                Some(value) => Some((name, value)),
            })
            .collect()
    }

    /// The consts whose expression evaluated to none, so their value is
    /// the outer scope's: each const's expression output
    /// (`__init_<name>`) pulled on a fork.
    fn silent_fall_throughs(&self) -> Vec<String> {
        let mut probe: Option<Box<dyn Kernel>> = None;
        let inits = self.kernel.const_inits();
        self.program
            .const_outputs()
            .into_iter()
            .filter(|name| {
                let own = inits
                    .iter()
                    .find(|c| c.name == *name)
                    .map_or(*name, |c| c.source.as_str());
                let Some(idx) = self.program.output_index(own) else {
                    return false;
                };
                let probe = probe.get_or_insert_with(|| self.kernel.fork());
                matches!(probe.pull_at(idx), Value::None)
            })
            .map(str::to_string)
            .collect()
    }
}

/// What a holder of a shared scope accepts: a scope kernel, one already
/// shared, or an interpreter kernel standing in for a scope.
pub trait IntoSharedScope {
    /// The scope, shared.
    fn into_shared_scope(self) -> Arc<ScopeKernel>;
}

impl IntoSharedScope for ScopeKernel {
    fn into_shared_scope(self) -> Arc<ScopeKernel> {
        Arc::new(self)
    }
}

impl IntoSharedScope for Arc<ScopeKernel> {
    fn into_shared_scope(self) -> Arc<ScopeKernel> {
        self
    }
}

impl IntoSharedScope for PolydatKernel {
    fn into_shared_scope(self) -> Arc<ScopeKernel> {
        Arc::new(ScopeKernel::from(self))
    }
}

impl Lookup for ScopeKernel {
    fn lookup(&self, name: &str) -> Option<Value> {
        ScopeKernel::lookup(self, name)
    }

    fn ledger(&self) -> &Arc<polydat::kernel::CompileLedger> {
        self.kernel.ledger()
    }
}

/// Source matter for [`ScopeKernel::build_under`]: what
/// `PolydatMatter::builder().source(..)` carried.
pub struct SourceMatter {
    /// The scope's label, for diagnostics.
    pub label: String,
    /// The scope's body.
    pub body: polydat::kernel::subcontext::BodyFragment,
    /// Outputs the scope re-emits from its parent rather than declares.
    pub inherited_outputs: Vec<String>,
    /// Compile options.
    pub options: polydat::kernel::subcontext::CompileOptions,
    /// `result:` bindings, when the scope has any.
    pub result_bindings: Option<String>,
}

impl SourceMatter {
    /// Matter from Polydat source text.
    pub fn source(
        label: impl Into<String>,
        source: impl Into<String>,
        options: polydat::kernel::subcontext::CompileOptions,
    ) -> Self {
        Self {
            label: label.into(),
            body: polydat::kernel::subcontext::BodyFragment::PolydatSource(source.into()),
            inherited_outputs: Vec::new(),
            options,
            result_bindings: None,
        }
    }

    /// Matter from built statements (SRD-84 graph matter).
    pub fn statements(
        label: impl Into<String>,
        statements: Vec<polydat::dsl::ast::Statement>,
        options: polydat::kernel::subcontext::CompileOptions,
    ) -> Self {
        Self {
            label: label.into(),
            body: polydat::kernel::subcontext::BodyFragment::Statements(statements),
            inherited_outputs: Vec::new(),
            options,
            result_bindings: None,
        }
    }

    /// Mark `names` as re-emitted from the parent.
    pub fn inherited(mut self, names: Vec<String>) -> Self {
        self.inherited_outputs = names;
        self
    }

    /// Attach `result:` bindings.
    pub fn results(mut self, source: impl Into<String>) -> Self {
        self.result_bindings = Some(source.into());
        self
    }
}
