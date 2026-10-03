//! The **resource** port: turning files that are *not source code* into pipeline facts.
//!
//! A project's behaviour often lives in files the language parser never touches — MyBatis mapper XML, an ORM
//! mapping file, a code-generated descriptor. Those files are invisible to the graph: without them a project like
//! `mall` has zero table semantics, because every SQL statement sits in `resources/mapper/*.xml`. The kidney of a
//! resource adapter is therefore to synthesise **pseudo facts** the rule pipeline can consume like any other fact.
//!
//! This is the counterpart of [`crate::port::techstack`] (prepare-time knowledge), following the same contract:
//! the *port* lives here so both the kernel and the adapters can reference it without a dependency cycle, while
//! the *concrete adapter* lives in a `gt-adapter-*` crate that depends only on this one. The adapter returns
//! *data*; the kernel owns every `PipelineContext` mutation.
//!
//! Why this is a port of its own rather than another arm of `TechStackAdapter`:
//! * **Timing** — a resource fact must attach to nodes P2 already built (`owner` resolution below), so it is
//!   applied after P2/P3, whereas `TechStackAdapter::load` runs *inside* P3 and only ever returns loose facts;
//! * **Shape** — it contributes call-site-like records that feed P5's rules, not symbol-table rows
//!   ([`crate::port::techstack::AdapterFact`] carries `Schema` / `Symbol` only);
//! * **Cardinality** — `TechStackRegistry` resolves one adapter per *language*, while several resource flavours
//!   coexist inside one language (MyBatis XML *and* Hibernate mappings *and* JOOQ, say).
//!
//! What stays with knowledge (FKB): **whether** a given library applies, decided by its detectors — the kernel
//! never guesses, it only asks P3 whether that knowledge id was recognised for this sub-project.

use std::path::Path;

use crate::error::Result;
use crate::model::{FactValue, Span, SubProject};
use crate::port::FileSystem;

/// A call site the parser never saw, because it is written in a resource file.
///
/// It is deliberately shaped like a parser fact (`receiver` / `method` / `args`), so P5 matches it exactly like a
/// real call site — see the mybatis knowledge base, where `mybatis::select` selects it and `arg: 0` carries the
/// table name.
#[derive(Debug, Clone)]
pub struct PseudoCall {
    /// The FQN of the node this call site hangs from. The kernel resolves it (see `ResourceAdapter::scan`);
    /// `owner_class` is its fallback, exactly like a real call site's fallback chain.
    pub owner_fqn: String,
    /// The owning class FQN, used when the member node itself cannot be resolved.
    pub owner_class: Option<String>,
    /// Full callee spelling, e.g. `mybatis::select`. The matcher splits `A::b` into receiver + method, so both
    /// fields below must be filled faithfully (see `callee_matches`).
    pub callee: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    /// Call arguments — how a value reaches the rules (`arg: 0`).
    pub args: Vec<FactValue>,
    /// Location inside the resource file; only the line carries meaning here.
    pub span: Span,
    /// Display path of the resource file (project / sub-project relative, `/`-separated).
    pub file: String,
    /// The one-line evidence the UI shows next to the call site.
    pub snippet: String,
    /// Extra evidence properties merged into the call-site node (e.g. `{"mapper": "src/main/resources/…"}`), so
    /// the UI can verify where a synthesised fact came from. The key names here belong to the adapter, not to the
    /// kernel.
    pub props: serde_json::Value,
    /// How much the adapter trusts this synthesis (rules combine it further downstream).
    pub confidence: f32,
}

/// A fact produced by a resource adapter.
#[derive(Debug, Clone)]
pub enum ResourceFact {
    /// See [`PseudoCall`].
    PseudoCall(PseudoCall),
}

/// One library's resource files, turned into facts the pipeline can consume.
pub trait ResourceAdapter: Send + Sync {
    /// The **knowledge id** this adapter serves (`mybatis` today).
    ///
    /// The kernel runs the adapter only for sub-projects P3 recognised under this same id, which is what keeps
    /// "does this library apply" in the knowledge base and out of the kernel.
    fn id(&self) -> &str;

    /// Read this sub-project's resource files and return the facts they contain.
    ///
    /// Returns an empty `Vec` when nothing matches — a resource file that does not exist is not an error.
    fn scan(
        &self,
        sub: &SubProject,
        project_root: &Path,
        fs: &dyn FileSystem,
    ) -> Result<Vec<ResourceFact>>;
}

/// Factory port: the resource adapters known to this installation.
pub trait ResourceAdapterRegistry: Send + Sync {
    fn adapters(&self) -> Vec<&dyn ResourceAdapter>;
}

/// The default registry, holding every registered resource adapter.
pub struct DefaultResourceAdapterRegistry {
    adapters: Vec<Box<dyn ResourceAdapter>>,
}

impl DefaultResourceAdapterRegistry {
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// Register an adapter (chainable).
    pub fn register(mut self, adapter: Box<dyn ResourceAdapter>) -> Self {
        self.adapters.push(adapter);
        self
    }
}

impl Default for DefaultResourceAdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceAdapterRegistry for DefaultResourceAdapterRegistry {
    fn adapters(&self) -> Vec<&dyn ResourceAdapter> {
        self.adapters.iter().map(|a| a.as_ref()).collect()
    }
}
