//! Projects / sub-projects / source files.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ids::{FileId, ProjectId, SubProjectId};
use super::kinds::Language;

/// A top-level project under analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// The absolute path of the project root.
    pub root_path: PathBuf,
    pub description: Option<String>,
    /// Extra configuration (exclude patterns, the list of required locales, etc.) so nothing has to be hard-coded.
    pub config: ProjectConfig,
    pub status: ProjectStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input for creating a project (excluding server-generated fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProject {
    pub name: String,
    pub root_path: PathBuf,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// Input for updating a project; `None` means "do not change this field".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectPatch {
    pub name: Option<String>,
    pub root_path: Option<PathBuf>,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// Project-level configuration. Everything is overridable; nothing is hard-coded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Extra exclude globs (layered on top of the scanner's defaults and the ones framework knowledge
    /// resolves in P0).
    ///
    /// Matched as **globs against the path relative to the project root** (`public/static/**`,
    /// `storage/logs/**`), not as bare directory names.
    pub exclude_globs: Vec<String>,
    /// The list of locales required by the i18n coverage check.
    ///
    /// Defaults to `["en-us", "zh-cn"]`: English is the source language, so it comes first and the
    /// `missing_locales` subkind reports what is absent in that order. Projects that need a different
    /// set (or a single locale) override it explicitly — nothing is inferred from the codebase.
    pub required_locales: Vec<String>,
    /// Database table prefix (used for identity normalisation, e.g. `eb_`).
    ///
    /// Empty by default: the prefix should be given explicitly in the project config, or detected automatically by
    /// P3 from framework config (such as ThinkPHP's `config/database.php`). Never bake in any project-specific
    /// default (sample_project's `eb_` must not leak into the generic layer).
    pub table_prefixes: Vec<String>,
    /// Whether to run the full pipeline (when off, only Ingest + CfAst run).
    pub full_pipeline: bool,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            exclude_globs: Vec::new(),
            required_locales: vec!["en-us".into(), "zh-cn".into()],
            table_prefixes: Vec::new(),
            full_pipeline: true,
        }
    }
}

/// Project status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// Created, not graphed yet.
    Created,
    /// Graphing in progress.
    Indexing,
    /// Graphing finished.
    Ready,
    /// Graphing failed.
    Failed,
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Created => "created",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// A sub-project: an independently analysable unit inside a project.
///
/// For example, `CRMEB-master` contains both a ThinkPHP backend and a Uni-app frontend; they differ in language and
/// in FKB, so they must be analysed as two sub-projects and only converge across projects on contract nodes such as
/// `HttpContract`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubProject {
    pub id: SubProjectId,
    pub project_id: ProjectId,
    pub name: String,
    pub root_path: PathBuf,
    pub language: Language,
    /// Sub-project role, of the form `tier` or `tier:kind` (e.g. `backend` / `frontend:admin` / `backend:worker`).
    /// `tier` is one of `frontend` / `backend` / `library` / `unknown`; `kind` further distinguishes the type
    /// (mini program / admin console / mobile / API / Worker …), recognised by Ingest from the directory name.
    pub role: String,
    /// The recognition evidence, e.g. "composer.json".
    pub detected_by: String,
    /// Identifier list of the frameworks (matched by FKB, e.g. `["thinkphp", "uni-app"]`).
    pub frameworks: Vec<String>,
    /// Framework root information (AppRoot, etc.), back-filled by the Prepare phase after resolution via FKB.
    pub facts: serde_json::Value,
}

/// A new sub-project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSubProject {
    pub project_id: ProjectId,
    pub name: String,
    pub root_path: PathBuf,
    pub language: Language,
    pub role: String,
    pub detected_by: String,
    pub frameworks: Vec<String>,
    pub facts: serde_json::Value,
}

/// A source file to analyse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub id: FileId,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    /// Path relative to the project root (always `/`-separated).
    pub path: String,
    pub language: Language,
    pub size_bytes: u64,
    pub content_hash: String,
}

/// A new source file (for batch insertion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSourceFile {
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub path: String,
    pub language: Language,
    pub size_bytes: u64,
    pub content_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        serde_json::from_value(serde_json::to_value(v).expect("serialize")).expect("deserialize")
    }

    /// `ProjectConfig` is both the **API request body** (`gt-adapter-http::dto`) and a JSON column in SQLite —
    /// and the store reads it with `parse_json::<ProjectConfig>().unwrap_or_default()`, so a row whose JSON is
    /// missing a key must still load as the documented defaults rather than as an error.
    #[test]
    fn project_config_defaults_apply_when_keys_are_absent() {
        let d = ProjectConfig::default();
        assert!(d.exclude_globs.is_empty());
        assert!(
            d.table_prefixes.is_empty(),
            "no project-specific default may leak into the table prefixes (sample_project's `eb_` must not)"
        );
        assert!(d.full_pipeline, "the full pipeline runs by default");
        // The one non-empty default in an otherwise "nothing hard-coded" struct: the i18n coverage check needs
        // a baseline, and `gt-pipeline::Config` readers rely on exactly this pair.
        assert_eq!(d.required_locales, vec!["en-us".to_string(), "zh-cn".to_string()]);
        // English is the source language, so it leads the i18n baseline: `missing_locales` reports the
        // absent locales in this order, and a reader must not treat the first entry as "whatever came first".
        assert_eq!(
            d.required_locales.first().map(String::as_str),
            Some("en-us"),
            "English is the default i18n language; Chinese is only the second required locale"
        );

        let empty: ProjectConfig = serde_json::from_value(json!({})).expect("an empty config object must deserialise");
        assert!(empty.exclude_globs.is_empty());
        assert_eq!(empty.required_locales, d.required_locales);
        assert!(empty.table_prefixes.is_empty());
        assert!(empty.full_pipeline);

        // A row written before `table_prefixes` existed: the new keys fall back, the old key survives.
        let old_row: ProjectConfig = serde_json::from_value(json!({ "exclude_globs": ["public/static/**"] })).unwrap();
        assert_eq!(old_row.exclude_globs, vec!["public/static/**".to_string()]);
        assert!(old_row.table_prefixes.is_empty());
        assert_eq!(old_row.required_locales, d.required_locales);

        let explicit: ProjectConfig = serde_json::from_value(json!({
            "exclude_globs": ["storage/logs/**"],
            "required_locales": ["fr-fr"],
            "table_prefixes": ["eb_"],
            "full_pipeline": false
        }))
        .unwrap();
        assert_eq!(explicit.exclude_globs, vec!["storage/logs/**".to_string()]);
        assert_eq!(explicit.required_locales, vec!["fr-fr".to_string()]);
        assert_eq!(explicit.table_prefixes, vec!["eb_".to_string()]);
        assert!(!explicit.full_pipeline, "an explicit false must not be overwritten by the default");

        assert_eq!(round_trip(&explicit).required_locales, explicit.required_locales);
    }

    /// `ProjectPatch` is the PATCH body: `None` means "do not touch this field", so an absent key and an
    /// explicit `null` both mean "no change" — there is deliberately no way to clear a field through a patch.
    #[test]
    fn project_patch_treats_absent_and_null_as_no_change() {
        let d = ProjectPatch::default();
        assert!(d.name.is_none() && d.root_path.is_none() && d.description.is_none() && d.config.is_none());

        let empty: ProjectPatch = serde_json::from_value(json!({})).expect("an empty PATCH body must deserialise");
        assert!(empty.name.is_none() && empty.config.is_none());

        let partial: ProjectPatch = serde_json::from_value(json!({ "name": "renamed" })).unwrap();
        assert_eq!(partial.name.as_deref(), Some("renamed"));
        assert!(partial.root_path.is_none(), "a field that was not submitted must stay None");
        assert!(partial.description.is_none());
        assert!(partial.config.is_none());

        let nulled: ProjectPatch = serde_json::from_value(json!({ "description": null })).unwrap();
        assert!(
            nulled.description.is_none(),
            "null counts as 'not submitted': a patch cannot clear a field"
        );

        let full: ProjectPatch = serde_json::from_value(json!({ "config": { "full_pipeline": false } })).unwrap();
        let cfg = full.config.as_ref().expect("config must be replaceable as a whole");
        assert!(!cfg.full_pipeline);
    }

    /// The status written into the DB column is **the `Display` string** (`set_project_status` stores
    /// `ProjectStatus::Created.to_string()`) and is read back by a literal match, so `Display` must keep
    /// producing exactly the snake_case name that storage expects.
    #[test]
    fn project_status_display_matches_the_persisted_spelling() {
        for (variant, text) in [
            (ProjectStatus::Created, "created"),
            (ProjectStatus::Indexing, "indexing"),
            (ProjectStatus::Ready, "ready"),
            (ProjectStatus::Failed, "failed"),
        ] {
            assert_eq!(variant.to_string(), text);
            assert_eq!(serde_json::to_value(variant).unwrap(), json!(text));
            assert_eq!(serde_json::from_value::<ProjectStatus>(json!(text)).unwrap(), variant);
        }
        assert!(
            serde_json::from_value::<ProjectStatus>(json!("done")).is_err(),
            "an unknown status spelling must be rejected"
        );
        assert_eq!(ProjectStatus::Ready, ProjectStatus::Ready);
        assert_ne!(ProjectStatus::Created, ProjectStatus::Failed);
    }

    /// The project / sub-project / source-file records: `PathBuf` must stay a plain string on the wire, and the
    /// free-form `facts` payload must not be reshaped.
    #[test]
    fn project_sub_project_and_source_file_records_round_trip() {
        let p = Project {
            id: ProjectId(1),
            name: "sample_project".to_string(),
            root_path: PathBuf::from("/samples/php-projects/thinkphp/sample_project"),
            description: Some("e-commerce".to_string()),
            config: ProjectConfig { full_pipeline: false, ..Default::default() },
            status: ProjectStatus::Ready,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_123,
        };
        let back: Project = round_trip(&p);
        assert_eq!(back.id, ProjectId(1));
        assert_eq!(back.root_path, PathBuf::from("/samples/php-projects/thinkphp/sample_project"));
        assert_eq!(back.description.as_deref(), Some("e-commerce"));
        assert_eq!(back.status, ProjectStatus::Ready);
        assert_eq!((back.created_at, back.updated_at), (1_700_000_000, 1_700_000_123));
        assert!(!back.config.full_pipeline);
        let serialized = serde_json::to_value(&p).unwrap();
        assert!(
            serialized["root_path"].is_string(),
            "PathBuf must serialise as a bare string, not as an object"
        );

        // A project created without any configuration must not fail to load later.
        let np = NewProject {
            name: "bagisto".to_string(),
            root_path: PathBuf::from("/samples/bagisto"),
            description: None,
            config: None,
        };
        let back: NewProject = round_trip(&np);
        assert_eq!(back.name, "bagisto");
        assert!(back.description.is_none() && back.config.is_none());

        let sp = SubProject {
            id: SubProjectId(2),
            project_id: ProjectId(1),
            name: "uni-app".to_string(),
            root_path: PathBuf::from("template/uni-app"),
            language: Language::new("javascript"),
            role: "frontend:admin".to_string(),
            detected_by: "pages.json".to_string(),
            frameworks: vec!["uni-app".to_string()],
            facts: json!({ "app_root": "src" }),
        };
        let back: SubProject = round_trip(&sp);
        assert_eq!(back.id, SubProjectId(2));
        assert_eq!(back.role, "frontend:admin", "role is `tier` or `tier:kind`, stored and read verbatim");
        assert_eq!(back.detected_by, "pages.json");
        assert_eq!(back.frameworks, vec!["uni-app".to_string()]);
        assert_eq!(back.facts, json!({ "app_root": "src" }));

        let nsp = NewSubProject {
            project_id: ProjectId(1),
            name: "api".to_string(),
            root_path: PathBuf::from("app/api"),
            language: Language::new("php"),
            role: "backend".to_string(),
            detected_by: "composer.json".to_string(),
            frameworks: vec!["thinkphp".to_string(), "sample_project".to_string()],
            facts: json!(null),
        };
        let back: NewSubProject = round_trip(&nsp);
        assert_eq!(back.frameworks, vec!["thinkphp".to_string(), "sample_project".to_string()]);
        assert!(back.facts.is_null());

        // A file outside any sub-project (`sub_project_id: None`) is legal — it simply belongs to the project.
        let f = SourceFile {
            id: FileId(9),
            project_id: ProjectId(1),
            sub_project_id: None,
            path: "app/api/controller/Login.php".to_string(),
            language: Language::new("php"),
            size_bytes: 4096,
            content_hash: "deadbeef".to_string(),
        };
        let back: SourceFile = round_trip(&f);
        assert_eq!(back.id, FileId(9));
        assert!(back.sub_project_id.is_none());
        assert_eq!(back.path, "app/api/controller/Login.php");
        assert_eq!(back.size_bytes, 4096);
        assert_eq!(back.content_hash, "deadbeef");

        let nf = NewSourceFile {
            project_id: ProjectId(1),
            sub_project_id: Some(SubProjectId(2)),
            path: "app/model/User.php".to_string(),
            language: Language::new("php"),
            size_bytes: 1,
            content_hash: String::new(),
        };
        let back: NewSourceFile = round_trip(&nf);
        assert_eq!(back.sub_project_id, Some(SubProjectId(2)));
        assert!(back.content_hash.is_empty());
    }
}
