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
use crate::model::{EntryField, Language, MethodRefSpec, SubProject};
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

/// One entry of a config collection, e.g. a single database connection.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    /// The entry's own name (e.g. the connection name `mysql`).
    pub key: String,
    /// `(field name, value)` pairs, in the order FKB declared them. Fields that could not be
    /// resolved are **omitted** rather than filled with a guess.
    pub fields: Vec<(String, String)>,
}

/// A whole config collection plus, when FKB asked for it, the name of the default entry.
#[derive(Debug, Clone, Default)]
pub struct ManifestEntries {
    pub entries: Vec<ManifestEntry>,
    /// The name of the default entry, when the file declares one (`'default' => 'mysql'`).
    pub default: Option<String>,
}

/// A single tech-stack's prepare-time knowledge for one language.
///
/// Every method except [`TechStackAdapter::language`] has a neutral default, so an adapter only
/// implements the conventions its own ecosystem actually has (mirrors `LanguageParser`, where
/// `member_separator` / `manifest_files` / `exclude_dirs` are all defaulted).
pub trait TechStackAdapter: Send + Sync {
    /// The language this adapter is responsible for.
    fn language(&self) -> Language;

    /// Whether this adapter also serves `language` beyond [`TechStackAdapter::language`].
    ///
    /// Needed because one ecosystem spans several language tags: the JavaScript adapter serves
    /// `typescript` too, and a language tag must not silently degrade an ecosystem to "no adapter".
    fn serves(&self, language: &Language) -> bool {
        *language == self.language()
    }

    /// The dependency names declared by one of this stack's **manifest** files (e.g. composer.json's
    /// `require` / `require-dev` keys).
    ///
    /// `None` means "this stack does not recognise that file" — the kernel then falls back to a
    /// plain-text probe, so an ecosystem without an adapter degrades to over-detection rather than
    /// to silence (missing a framework is worse than mistaking one).
    fn manifest_dependencies(&self, _file_name: &str, _text: &str) -> Option<Vec<String>> {
        None
    }

    /// The dependency names in one of this stack's **lock** files — the resolved closure, so it
    /// covers packages the project never declared itself.
    ///
    /// Same `None` contract as [`TechStackAdapter::manifest_dependencies`].
    fn lock_dependencies(&self, _file_name: &str, _text: &str) -> Option<Vec<String>> {
        None
    }

    /// Where the **locale** sits in an i18n file's path: `前缀{locale}后缀` templates, prefix and suffix
    /// matched **literally** (no wildcards) — so write `lang/{locale}/` rather than `lang/{locale}/*.php`.
    ///
    /// Tried in order, first match wins — so the directory form must precede the flat-file form
    /// (`src/locales/{locale}/` before `src/locales/{locale}.json`). Empty means "this stack has no i18n
    /// path convention", and no locale is stamped (better absent than guessed).
    fn i18n_path_patterns(&self) -> Vec<String> {
        Vec::new()
    }

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

    /// Enumerate a family of entries in a config file (e.g. every connection under `connections.*`)
    /// and read the declared fields from each.
    ///
    /// The kernel knows neither the file format nor which keys carry the driver / prefix — both come
    /// from FKB (`root`, `fields`), so this stays language-shaped input, format-shaped work.
    fn read_manifest_entries(
        &self,
        _sub: &SubProject,
        _project_root: &Path,
        _manifest: &str,
        _root: &str,
        _fields: &[EntryField],
        _default_from: Option<&str>,
        _fs: &dyn FileSystem,
        _parsers: &dyn ParserRegistry,
    ) -> Option<ManifestEntries> {
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
        self.adapters.iter().find(|a| a.serves(language)).map(|a| a.as_ref())
    }
}

/// A sub-project detection marker: a package manifest file that identifies one language's ecosystem.
///
/// This is **bootstrap** knowledge — Ingest must decide a sub-project's language *before* any FKB is
/// loaded, so it cannot come from FKB. It arrives with the tech-stack registration, so the kernel knows
/// no concrete language here either.
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
