use serde::{Deserialize, Serialize};
use serde_json::Value;
/// A framework recognition signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Detector {
    /// A dependency exists in the manifest file.
    ManifestDependency {
        manifest: String,
        dependency: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// A characteristic file / directory exists.
    FileExists {
        path: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// A fully-qualified symbol is **imported** by some source file of the sub-project
    /// (`use GuzzleHttp\Client;`).
    ///
    /// This is **code evidence**, not manifest evidence, and it is what a manifest structurally cannot
    /// provide: a manifest only says what was *installed*, never what was *used*, and it is blind to
    /// anything pulled in transitively — `illuminate/*` never appears in a Laravel app's own
    /// `composer.json`, yet it is everywhere in its code. Writing `use` is also alias-proof:
    /// `use GuzzleHttp\Client as G;` records the same FQN, so renaming the local alias changes nothing.
    ///
    /// Matching: exact FQN, or a prefix when the pattern ends with `*` (`GuzzleHttp\*`). Both sides are
    /// compared lowercased and stripped of a leading separator, so `GuzzleHttp\Client` and
    /// `\GuzzleHttp\Client` are the same thing.
    ImportExists {
        symbol: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// A dependency exists in the **lock file** (`composer.lock`, `package-lock.json` …), i.e. in the
    /// resolved dependency closure rather than in the hand-written manifest.
    ///
    /// This is the only manifest-shaped signal that can see transitive dependencies: `composer.lock` lists
    /// every package actually installed, including the ones only `laravel/framework` asked for. It cannot
    /// tell you whether the code *uses* the package though — prefer [`Detector::ImportExists`] for that, and
    /// keep this for libraries that are consumed without a distinctive import.
    LockDependency {
        lock: String,
        dependency: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// A call site matching this pattern occurs in the sub-project's source.
    ///
    /// Same grammar as a rule's `selector.callee` (`|` alternatives, `A::b` with `*` tail matching, bare
    /// method names). Use it for libraries that leave no import behind — a PHP fully-qualified inline call
    /// (`\GuzzleHttp\Client::request()`), or a JS global — and pair it with [`Detector::ImportExists`]
    /// so either spelling activates the knowledge.
    CallExists {
        callee: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
}

impl Detector {
    /// The confidence this signal carries when it matches.
    ///
    /// A method rather than a `match` at each call site, because a call-site match silently defaults new
    /// variants to whatever the last arm says — that is exactly the kind of bug this accessor removes.
    pub fn confidence(&self) -> f32 {
        match self {
            Detector::ManifestDependency { confidence, .. }
            | Detector::FileExists { confidence, .. }
            | Detector::ImportExists { confidence, .. }
            | Detector::LockDependency { confidence, .. }
            | Detector::CallExists { confidence, .. } => *confidence,
        }
    }
}

pub(crate) fn default_conf() -> f32 {
    0.9
}

/// Framework root resolution rules.
///
/// Example: resolving `AppRoot = "app"` from `autoload.psr-4` in `composer.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRule {
    pub id: String,
    /// The fact key produced, e.g. `app_root`.
    pub key: String,
    pub source: RootSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
    /// Fallback candidate directories; probed in order when resolution fails.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RootSource {
    /// Read a value from a JSON manifest by pointer.
    ManifestJson {
        manifest: String,
        /// Dotted path, e.g. `autoload.psr-4`.
        pointer: String,
        /// Value-selection strategy.
        pick: PickStrategy,
    },
    /// Probe directly whether a directory exists.
    DirectoryExists { path: String },
    /// Read a value from a project manifest / config file (e.g. ThinkPHP's `config/database.php`) by
    /// dotted pointer. The *interpretation* of the file format is delegated to the tech-stack adapter
    /// (`TechStackAdapter::read_manifest`), so the kernel knows no language-specific file format.
    Manifest {
        /// Path relative to the project root, e.g. `config/database.php`.
        manifest: String,
        /// Dotted path, e.g. `connections.mysql.prefix`.
        pointer: String,
    },
    /// Enumerate a **family of entries** in a config file and read a set of fields from each one —
    /// e.g. every connection under `connections.*` in `config/database.php`, each contributing its own
    /// driver and table prefix.
    ///
    /// This exists because a single `pointer` cannot express "there may be several of these": projects
    /// configure read/write splitting (`mysql` / `mysql_read`) or several databases, and the connection
    /// **name** is user-chosen — it is not always `mysql`. Which key holds the driver (`type` on
    /// ThinkPHP, `driver` on Laravel) and which holds the prefix is framework knowledge, so it stays
    /// here; the file format itself is the adapter's job, as with [`RootSource::Manifest`].
    ///
    /// The fact produced is a **list** (one record per entry), so this kind cannot be substituted into
    /// a single `{value}` / `{<key>}` placeholder (P0 exclude rules): `facts::resolve_root_source`
    /// returns `None` for it.
    ManifestEntries {
        /// Path relative to the project root, e.g. `config/database.php`.
        manifest: String,
        /// The key whose children are the entries, e.g. `connections`.
        root: String,
        /// Fields read from every entry.
        fields: Vec<EntryField>,
        /// Pointer, relative to the file root, giving the **default** entry's name, e.g. `default`
        /// (`'default' => env('DB_CONNECTION', 'mysql')`).
        #[serde(default)]
        default_from: Option<String>,
    },
}

/// One field read from each entry of a [`RootSource::ManifestEntries`] collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryField {
    /// Field name in the produced record, e.g. `driver` / `prefix`.
    pub name: String,
    /// Pointer **relative to `root`**; `{key}` is replaced by the entry's name, so with
    /// `root: connections` and `pointer: "{key}.prefix"` the resolved path is `connections.mysql.prefix`.
    #[serde(default)]
    pub pointer: Option<String>,
    /// Take a property of the entry itself instead of a pointer value.
    #[serde(default)]
    pub from: Option<EntryFieldFrom>,
}

/// A property of an entry itself, as opposed to a value found under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryFieldFrom {
    /// The entry's own name (e.g. the connection name `mysql`).
    Key,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickStrategy {
    /// Take the shallowest of all mapped directories.
    ShallowestDir,
    /// Take the first mapped directory.
    FirstDir,
    /// Take the mapped directory whose key name equals the given value.
    ByNamespaceKey,
}

/// One directory / file to keep **out of the scan**.
///
/// Declared by FKB rather than hard-coded in the kernel, because only framework knowledge knows that
/// ThinkPHP drops its cache under `runtime/`, that Laravel compiles Blade into `storage/framework/`,
/// or that a project may have moved `public/` to `web/`.
///
/// Resolution happens in **P0, before the scan**, so a rule may only use sources that need no graph —
/// the same three [`RootSource`] kinds `root_rules` use. The glob template may reference them:
/// * `{value}` — this rule's own `source`;
/// * `{<key>}` — the value of the `root_rules` entry whose `key` is `<key>` (e.g. `{app_root}`).
///
/// Globs are relative to the **sub-project root**; a `/**` suffix means "the whole subtree".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludeRule {
    pub id: String,
    /// Glob template, e.g. `{app_root}/runtime/**`.
    pub glob: String,
    /// Where the value behind `{value}` comes from (e.g. `composer.json`'s `extra.public-dir`).
    #[serde(default)]
    pub source: Option<RootSource>,
    /// Candidates tried in order when the template cannot be rendered. Each is kept **only when the
    /// directory it points at really exists**, so a wrong guess can never delete real source code.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

/// The `{placeholder}` names a glob template references, in order of appearance and de-duplicated.
pub fn template_placeholders(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let name = after[..end].trim().to_string();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        rest = &after[end + 1..];
    }
    out
}

/// A P3 symbol-table loader.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderSpec {
    pub id: String,
    /// Output symbol-table name: `schema` / `config_keys` / `i18n` / `facade_map` / `route_list`, etc.
    pub table: String,
    pub from: LoaderSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

/// Which parser is used is decided by the **file's extension** (`LanguageParser` registry), not by a
/// format field — so a `.php` config array, a `.json` and a `.yml` are each parsed by their own parser
/// with no per-loader declaration. (A `format:` field used to exist here but was never read; declaring
/// something the kernel does not consume silently misleads FKB authors, so it was removed.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoaderSource {
    /// Read a single file and take values by key_path.
    File {
        path: String,
        #[serde(default)]
        key_path: Option<String>,
    },
    /// Read in bulk by glob (e.g. `lang/*/*.php`); capture groups can extract the locale.
    Glob {
        pattern: String,
        /// Regex that extracts the locale from a path capture group (the first capture group).
        #[serde(default)]
        locale_regex: Option<String>,
    },
    /// A constant table declared inline by FKB (e.g. FacadeMap — given by framework knowledge, not guessed).
    Inline { rows: Vec<Value> },
    /// A built-in loader (implemented by the pipeline, e.g. collecting `$table` and `Db::name` from PHP source).
    Builtin {
        name: String,
        #[serde(default)]
        params: Value,
    },
}
