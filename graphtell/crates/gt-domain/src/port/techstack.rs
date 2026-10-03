//! The tech-stack-specific *prepare* port.
//!
//! A tech-stack adapter turns a language / ecosystem's concrete conventions (composer.json PSR-4,
//! PHP `return [...]` config, Laravel `Schema::create` migrations, …) into the kernel's
//! language-agnostic representation. The kernel's prepare phase stays generic and only knows this port.
//!
//! This mirrors `parsing.rs` (`LanguageParser`): the *port* lives in the domain (so it can be referenced
//! by both the kernel and the adapters without a dependency cycle), while the *concrete adapter* lives in
//! a `gt-adapter-*` crate that depends only on `gt-domain`. The composition root wires the two.
//!
//! The adapter returns *data* (`AdapterFact`) rather than mutating `PipelineContext`, which is what lets
//! the port live here without pulling in the pipeline crate.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::{Language, MethodRefSpec, SubProject};
use crate::port::{FileSystem, ParserRegistry};

/// A fact the kernel should persist into the workspace on behalf of a tech-stack adapter.
///
/// The adapter produces these; the kernel applies them (so the kernel keeps ownership of all
/// `PipelineContext` mutation, and the adapter never sees the workspace).
#[derive(Debug, Clone)]
pub enum AdapterFact {
    /// A schema entry: `table -> columns`, plus the source file for provenance.
    /// The kernel strips the table prefix before merging.
    Schema {
        table: String,
        columns: Vec<String>,
        source: String,
    },
    /// A generic symbol-table entry: `(table, key, value)`.
    Symbol {
        table: String,
        key: String,
        value: serde_json::Value,
    },
}

/// A single tech-stack's prepare-time knowledge for one language.
///
/// Every method except [`TechStackAdapter::language`] has a neutral default, so an adapter only
/// implements the conventions its own ecosystem actually has (mirrors `LanguageParser`, where
/// `member_separator` / `manifest_files` / `exclude_dirs` are all defaulted).
pub trait TechStackAdapter: Send + Sync {
    /// The language this adapter is responsible for.
    fn language(&self) -> Language;

    /// PSR-4 (or equivalent) namespace → directory roots for this ecosystem, derived from its
    /// package manifest (e.g. composer.json `autoload.psr-4`). The kernel never assumes a hard-coded
    /// directory name like `Http/Controllers` / `controller`.
    fn manifest_namespaces(&self, _sub_root: &Path, _project_root: &Path) -> Vec<(String, PathBuf)> {
        Vec::new()
    }

    /// Read an authoritative value from a project manifest / config file (e.g. composer.json PSR-4,
    /// a PHP `return [...]` config) by a dotted pointer.
    fn read_manifest(
        &self,
        _sub: &SubProject,
        _project_root: &Path,
        _manifest: &str,
        _pointer: &str,
        _fs: &dyn FileSystem,
        _parsers: &dyn ParserRegistry,
    ) -> Option<(String, String)> {
        None
    }

    /// Enrich a `method_ref` spec with this stack's namespace / app-module knowledge (learned from the
    /// real class FQNs on the graph, not from a hard-coded controller directory name).
    fn enrich_method_ref(&self, _spec: &mut MethodRefSpec, _sub_root: &Path, _project_root: &Path) {}

    /// Run a named built-in loader and return the facts to persist.
    ///
    /// The kernel dispatches the loader `id` here first; the adapter returns `Ok(vec![])` for ids it
    /// does not handle, so the kernel can fall back to its generic (params-driven) built-in loaders.
    ///
    /// `table_prefixes` is the full prefix list known so far (detected + FKB-declared) so the adapter
    /// can strip it from table names before returning a `Schema` fact.
    fn load(
        &self,
        _loader_id: &str,
        _params: &serde_json::Value,
        _sub: &SubProject,
        _project_root: &Path,
        _fs: &dyn FileSystem,
        _parsers: &dyn ParserRegistry,
        _table_prefixes: &[String],
    ) -> Result<Vec<AdapterFact>> {
        Ok(Vec::new())
    }

    /// Recognise a sub-project's **type** from this ecosystem's own config, for when the directory
    /// name alone is not conclusive (e.g. a JS project whose `package.json` declares `react-native`
    /// → `mobile`, or uni-app plus a `uni-mp-*` compiler → `mini-program`).
    ///
    /// This is only the **fallback**: the kernel first tries its language-agnostic directory-name
    /// heuristics. Returning `None` means "this stack cannot tell" and the kernel keeps the bare tier.
    fn sub_project_kind(&self, _dir: &Path, _fs: &dyn FileSystem) -> Option<String> {
        None
    }
}

/// Factory port: resolve the adapter that matches a sub-project's language.
pub trait TechStackRegistry: Send + Sync {
    fn adapter_for(&self, language: &Language) -> Option<&dyn TechStackAdapter>;
}

/// The default registry, holding one adapter per language.
pub struct DefaultTechStackRegistry {
    adapters: Vec<Box<dyn TechStackAdapter>>,
}

impl DefaultTechStackRegistry {
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// Register an adapter (chainable).
    pub fn register(mut self, adapter: Box<dyn TechStackAdapter>) -> Self {
        self.adapters.push(adapter);
        self
    }
}

impl Default for DefaultTechStackRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TechStackRegistry for DefaultTechStackRegistry {
    fn adapter_for(&self, language: &Language) -> Option<&dyn TechStackAdapter> {
        self.adapters
            .iter()
            .find(|a| a.language() == *language)
            .map(|a| a.as_ref())
    }
}

/// A sub-project detection marker: a package manifest file that identifies one language's ecosystem.
///
/// This is **bootstrap** knowledge — Ingest must decide a sub-project's language *before* any FKB is
/// loaded, so it cannot come from FKB. It used to be a hard-coded table inside the kernel
/// (`ingest::MARKERS`, e.g. `composer.json` → php / backend), which meant adding a language always meant
/// changing the kernel. It now arrives with the tech-stack registration, so the kernel knows no
/// concrete language here either.
#[derive(Debug, Clone)]
pub struct Marker {
    /// Marker file name, e.g. `composer.json`.
    pub file: String,
    /// The language this marker identifies.
    pub language: Language,
    /// The default system tier / role, e.g. `backend` / `frontend`.
    pub role: String,
}

/// Factory port: the sub-project detection markers known to this installation.
pub trait MarkerProvider: Send + Sync {
    fn markers(&self) -> Vec<Marker>;
}
