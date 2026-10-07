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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------- template_placeholders (the only logic-bearing fn)

    /// Placeholders are returned in order of appearance and de-duplicated.
    #[test]
    fn template_placeholders_in_order_and_dedup() {
        assert_eq!(
            template_placeholders("{value}{app_root}"),
            vec!["value".to_string(), "app_root".to_string()]
        );
        assert_eq!(
            template_placeholders("{value}{value}"),
            vec!["value".to_string()],
            "a repeated placeholder appears once"
        );
        assert_eq!(
            template_placeholders("pre{value}mid{app_root}post"),
            vec!["value".to_string(), "app_root".to_string()],
            "text around braces is ignored"
        );
    }

    /// No braces, empty input, an unmatched `{`, and an empty `{}` all yield no placeholders.
    #[test]
    fn template_placeholders_empty_and_negative() {
        assert!(template_placeholders("").is_empty(), "empty template -> no placeholders");
        assert!(
            template_placeholders("no braces here").is_empty(),
            "a template with no braces yields nothing"
        );
        assert!(
            template_placeholders("{value").is_empty(),
            "an unmatched `{{` yields no placeholder (never a partial)"
        );
        assert!(
            template_placeholders("{}").is_empty(),
            "an empty `{{}}` is not a placeholder"
        );
    }

    /// Whitespace inside the braces is trimmed, so `{ value }` is the `value` placeholder.
    #[test]
    fn template_placeholders_trims_whitespace() {
        assert_eq!(
            template_placeholders("{  value  }"),
            vec!["value".to_string()]
        );
    }

    // ------------------------------------------------------- Detector::confidence / default_conf

    /// `confidence()` reads the per-variant field; the `default_conf()` fallback (0.9) applies when the
    /// field is omitted on deserialization — never a hard-coded magic number at the call site.
    #[test]
    fn detector_confidence_per_variant_and_default() {
        assert_eq!(default_conf(), 0.9);

        let explicit = Detector::FileExists {
            path: "x".into(),
            confidence: 0.5,
        };
        assert_eq!(explicit.confidence(), 0.5);

        let via_default: Detector = serde_json::from_value(json!({
            "kind": "file_exists", "path": "x"
        }))
        .unwrap();
        assert_eq!(via_default.confidence(), 0.9, "omitted confidence falls back to default_conf");

        // Every variant exposes the same accessor (the match is exhaustive, so a new variant is a compile error).
        for d in [
            Detector::ManifestDependency { manifest: "c".into(), dependency: "d".into(), confidence: 0.3 },
            Detector::ImportExists { symbol: "s".into(), confidence: 0.4 },
            Detector::LockDependency { lock: "l".into(), dependency: "d".into(), confidence: 0.6 },
            Detector::CallExists { callee: "c".into(), confidence: 0.7 },
        ] {
            assert!((d.confidence() > 0.0 && d.confidence() < 1.0));
        }
    }

    // ------------------------------------------------------- snake_case variant tags (positive round-trips)

    #[test]
    fn detector_variant_tags_are_snake_case() {
        assert_eq!(
            serde_json::to_value(Detector::ManifestDependency {
                manifest: "c".into(),
                dependency: "d".into(),
                confidence: 0.9
            })
            .unwrap()["kind"],
            json!("manifest_dependency")
        );
        assert_eq!(
            serde_json::to_value(Detector::FileExists { path: "x".into(), confidence: 0.9 }).unwrap()["kind"],
            json!("file_exists")
        );
        assert_eq!(
            serde_json::to_value(Detector::ImportExists { symbol: "s".into(), confidence: 0.9 }).unwrap()["kind"],
            json!("import_exists")
        );
        assert_eq!(
            serde_json::to_value(Detector::LockDependency {
                lock: "l".into(),
                dependency: "d".into(),
                confidence: 0.9
            })
            .unwrap()["kind"],
            json!("lock_dependency")
        );
        assert_eq!(
            serde_json::to_value(Detector::CallExists { callee: "c".into(), confidence: 0.9 }).unwrap()["kind"],
            json!("call_exists")
        );
    }

    #[test]
    fn root_source_and_loader_source_tags_are_snake_case() {
        assert_eq!(
            serde_json::to_value(RootSource::DirectoryExists { path: "app".into() }).unwrap()["kind"],
            json!("directory_exists")
        );
        assert_eq!(
            serde_json::to_value(RootSource::ManifestJson {
                manifest: "c".into(),
                pointer: "p".into(),
                pick: PickStrategy::FirstDir,
            })
            .unwrap()["kind"],
            json!("manifest_json")
        );
        assert_eq!(
            serde_json::to_value(LoaderSource::File { path: "x".into(), key_path: None }).unwrap()["kind"],
            json!("file")
        );
        assert_eq!(
            serde_json::to_value(LoaderSource::Inline { rows: vec![json!({"k": "v"})] }).unwrap()["kind"],
            json!("inline")
        );
    }

    #[test]
    fn pick_strategy_and_entry_field_from_snake_case() {
        assert_eq!(serde_json::to_value(PickStrategy::ShallowestDir).unwrap(), json!("shallowest_dir"));
        assert_eq!(serde_json::to_value(PickStrategy::FirstDir).unwrap(), json!("first_dir"));
        assert_eq!(
            serde_json::to_value(PickStrategy::ByNamespaceKey).unwrap(),
            json!("by_namespace_key")
        );
        // Round-trip both ways.
        assert_eq!(
            serde_json::from_value::<PickStrategy>(json!("by_namespace_key")).unwrap(),
            PickStrategy::ByNamespaceKey
        );
        assert_eq!(serde_json::to_value(EntryFieldFrom::Key).unwrap(), json!("key"));
        assert_eq!(
            serde_json::from_value::<EntryFieldFrom>(json!("key")).unwrap(),
            EntryFieldFrom::Key
        );
    }

    // ------------------------------------------------------- deny_unknown_fields (unknown key = load error, never silent)

    /// A field the model does not have is a field the kernel cannot read; `deny_unknown_fields` turns a
    /// silent no-op into a hard load error — these tests lock that contract in for every struct/enum here.
    #[test]
    fn detector_unknown_field_rejected() {
        assert!(serde_json::from_value::<Detector>(
            json!({ "kind": "file_exists", "path": "x", "bogus": 1 })
        )
        .is_err());
    }

    #[test]
    fn root_rule_unknown_field_rejected() {
        assert!(serde_json::from_value::<RootRule>(json!({
            "id": "r", "key": "app_root",
            "source": { "kind": "directory_exists", "path": "app" },
            "bogus": 1
        }))
        .is_err());
    }

    #[test]
    fn root_source_unknown_variant_and_field_rejected() {
        assert!(serde_json::from_value::<RootSource>(json!({ "kind": "no_such_source" })).is_err());
        assert!(serde_json::from_value::<RootSource>(json!({
            "kind": "directory_exists", "path": "app", "extra": 1
        }))
        .is_err());
    }

    #[test]
    fn entry_field_unknown_field_rejected() {
        assert!(serde_json::from_value::<EntryField>(json!({
            "name": "driver", "pointer": "x.driver", "bogus": 1
        }))
        .is_err());
    }

    #[test]
    fn exclude_rule_unknown_field_rejected() {
        assert!(serde_json::from_value::<ExcludeRule>(json!({
            "id": "e", "glob": "{app_root}/runtime/**", "bogus": 1
        }))
        .is_err());
    }

    #[test]
    fn loader_spec_format_field_removed_rejected() {
        // The retired `format:` key must still be rejected (a key the kernel cannot read must error, not
        // sit in FKB looking effective).
        assert!(serde_json::from_value::<LoaderSpec>(json!({
            "id": "l", "table": "t",
            "from": { "kind": "file", "path": "x" },
            "format": "php"
        }))
        .is_err());
    }

    #[test]
    fn loader_source_unknown_variant_and_key_path_default() {
        assert!(serde_json::from_value::<LoaderSource>(json!({ "kind": "bogus" })).is_err());
        // `key_path` defaults to None when omitted, and is read when present.
        let file: LoaderSource =
            serde_json::from_value(json!({ "kind": "file", "path": "x" })).unwrap();
        match file {
            LoaderSource::File { path, key_path } => {
                assert_eq!(path, "x");
                assert!(key_path.is_none(), "key_path defaults to None");
            }
            _ => panic!("expected File loader source"),
        }
        let file_kp: LoaderSource =
            serde_json::from_value(json!({ "kind": "file", "path": "x", "key_path": "db" })).unwrap();
        match file_kp {
            LoaderSource::File { path, key_path } => {
                assert_eq!(path, "x");
                assert_eq!(key_path.as_deref(), Some("db"));
            }
            _ => panic!("expected File loader source"),
        }
    }
}

