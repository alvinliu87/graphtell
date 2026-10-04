//! Resolving facts from a project's **own configuration** — no graph required.
//!
//! Shared by the two phases that need framework knowledge without having parsed any code:
//! * P0 ([`crate::phase::exclude`]): exclusion globs must be known *before* the scan runs, so the
//!   scan never ingests framework caches / generated files in the first place;
//! * P3 ([`crate::phase::prepare`]): `root_rules` resolution (`app_root`, `db_prefix` …).
//!
//! Everything here reads a manifest or probes a directory. None of it depends on parsed code, which is
//! exactly what makes P0 use possible — P0 is the phase that decides *which files exist at all*.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use gt_domain::model::{
    Detector, Language, PickStrategy, ProjectId, RootSource, SubProject, SubProjectId,
};
use gt_domain::port::{
    FileSystem, KnowledgeProvider, ManifestEntries, ParserRegistry, TechStackAdapter,
    TechStackRegistry,
};
use serde_json::Value;

/// Whether `dependency` is declared by the manifest at `path`.
///
/// **Which sections hold dependencies is ecosystem knowledge**: composer.json uses `require` /
/// `require-dev`, package.json uses `dependencies` / `devDependencies` … so it is asked of the
/// tech-stack adapter ([`TechStackAdapter::manifest_dependencies`]), never hard-coded here. An
/// ecosystem with no adapter (or one that does not recognise the file) degrades to a plain-text
/// probe rather than failing to match — missing a framework is worse than mistaking one.
pub fn manifest_has(
    path: &Path,
    dependency: &str,
    fs: &dyn FileSystem,
    techstack: &dyn TechStackRegistry,
    language: &Language,
) -> bool {
    declared_in(path, dependency, fs, techstack, language, |a, name, text| {
        a.manifest_dependencies(name, text)
    })
}

/// Whether `dependency` appears in a **lock file**: the resolved dependency closure, so it covers packages
/// the project never declared itself.
///
/// Unlike [`manifest_has`], which only ever looks at hand-written declarations, this reads what was actually
/// installed — the only manifest-shaped signal that can see a package pulled in transitively. The lock
/// layout (composer.lock's `packages[]`, package-lock's `packages{}` keyed by `node_modules/…`, its v1
/// `dependencies{}` …) is ecosystem knowledge, so it is asked of the tech-stack adapter
/// ([`TechStackAdapter::lock_dependencies`]); anything else (yarn.lock, poetry.lock …) falls back to a
/// plain-text probe rather than failing to match, the same conservative direction `manifest_has` takes.
pub fn lock_has(
    path: &Path,
    dependency: &str,
    fs: &dyn FileSystem,
    techstack: &dyn TechStackRegistry,
    language: &Language,
) -> bool {
    declared_in(path, dependency, fs, techstack, language, |a, name, text| {
        a.lock_dependencies(name, text)
    })
}

/// Shared body of [`manifest_has`] / [`lock_has`]: ask the ecosystem for the declared names, then fall
/// back to a whole-file text probe (the conservative direction: over-detect, never under-detect).
fn declared_in(
    path: &Path,
    dependency: &str,
    fs: &dyn FileSystem,
    techstack: &dyn TechStackRegistry,
    language: &Language,
    names_of: impl Fn(&dyn TechStackAdapter, &str, &str) -> Option<Vec<String>>,
) -> bool {
    if !fs.exists(path) {
        return false;
    }
    let Ok(text) = fs.read_to_string(path) else { return false };
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if let Some(adapter) = techstack.adapter_for(language) {
        if let Some(names) = names_of(adapter, file_name, &text) {
            let needle = dependency.to_ascii_lowercase();
            if names.iter().any(|n| {
                let n = n.to_ascii_lowercase();
                n == needle || n.contains(&needle)
            }) {
                return true;
            }
        }
    }
    text.contains(dependency)
}

/// The locale of an i18n file, taken from the **stack's** path convention
/// ([`TechStackAdapter::i18n_path_patterns`]).
///
/// Which directory holds the translations and which path segment is the locale differ per ecosystem
/// (`lang/zh-cn/*.php` vs `src/locales/en/translation.json`), so none of that is written here;
/// a stack that declares nothing simply has no locale stamped.
pub fn locale_of_path(
    path: &str,
    language: &Language,
    techstack: &dyn TechStackRegistry,
) -> Option<String> {
    let adapter = techstack.adapter_for(language)?;
    adapter
        .i18n_path_patterns()
        .iter()
        .find_map(|p| crate::engine::capture_locale(p, path))
}

/// Resolve one [`RootSource`] to `(value, provenance)`.
///
/// Each kind deliberately only needs a manifest or a directory: that is what lets P0 use this too.
/// A manifest is looked up under the sub-project root first and the project root second (a single-module
/// project keeps its `composer.json` at the root, which *is* the sub-project root).
pub fn resolve_root_source(
    source: &RootSource,
    sub_root: &Path,
    project_root: &Path,
    sub: &SubProject,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Option<(String, String)> {
    match source {
        RootSource::ManifestJson { manifest, pointer, pick } => {
            let path = sub_root.join(manifest);
            let path = if fs.exists(&path) { path } else { project_root.join(manifest) };
            resolve_manifest_pointer(&path, pointer, *pick, fs)
        }
        RootSource::DirectoryExists { path } => resolve_directory_exists(sub_root, path),
        // The *interpretation* of the file format is the tech-stack adapter's job (PHP `return [...]`,
        // YAML, TOML …), so the kernel stays language-agnostic.
        RootSource::Manifest { manifest, pointer } => techstack
            .adapter_for(&sub.language)
            .and_then(|a| a.read_manifest(sub, project_root, manifest, pointer, fs, parsers)),
        // A collection resolves to **several** records, so it cannot fill a single placeholder;
        // list-shaped rules go through [`resolve_root_entries`] instead.
        RootSource::ManifestEntries { .. } => None,
    }
}

/// Resolve an entry-collection [`RootSource`] (e.g. every `connections.*` in `config/database.php`).
///
/// Separate from [`resolve_root_source`] because the result is a **list** of records, not one value:
/// a project may configure several database connections (read/write splitting, multiple databases) and
/// each carries its own driver and table prefix.
pub fn resolve_root_entries(
    source: &RootSource,
    project_root: &Path,
    sub: &SubProject,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Option<ManifestEntries> {
    let RootSource::ManifestEntries { manifest, root, fields, default_from } = source else {
        return None;
    };
    techstack.adapter_for(&sub.language).and_then(|a| {
        a.read_manifest_entries(
            sub,
            project_root,
            manifest,
            root,
            fields,
            default_from.as_deref(),
            fs,
            parsers,
        )
    })
}

/// A stand-in `SubProject` for callers that have not persisted sub-projects yet (P0).
///
/// Adapters only ever read `root_path` / `language` to locate the manifest, so an id of `0` is safe —
/// it is never written anywhere.
pub fn provisional_sub(sub_root: &Path, language: &Language) -> SubProject {
    SubProject {
        id: SubProjectId(0),
        project_id: ProjectId(0),
        name: String::new(),
        root_path: sub_root.to_path_buf(),
        language: language.clone(),
        role: "unknown".to_string(),
        detected_by: String::new(),
        frameworks: Vec::new(),
        facts: Value::Null,
    }
}

/// Probe whether `rel` exists inside `root`; on a hit return its **parent dir** as the value (the
/// semantics `app_root` needs: the source root, not `src/main/java` itself).
///
/// A single-module project hitting directly under the root returns `"."`; a multi-module project returns
/// the first hit module's relative dir (e.g. `mall-admin`). Only when the whole tree cannot be found do we
/// return `None` (triggering fallback / a diagnostic).
pub fn resolve_directory_exists(root: &Path, rel: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    // Hit directly under the root: source root is the sub-project root.
    if root.join(rel).is_dir() {
        return Some((".".to_string(), format!("directory exists: {}", rel)));
    }
    // Multi-module: breadth-first (prefer shallow), depth-limited to avoid scanning the whole large tree.
    let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::from([(root.to_path_buf(), 0)]);
    let max_depth = 5;
    while let Some((dir, depth)) = queue.pop_front() {
        let target = parts.iter().fold(dir.clone(), |acc, p| acc.join(p));
        if target.is_dir() {
            let rel_root = dir.strip_prefix(root).unwrap_or_else(|_| Path::new(""));
            let value = if rel_root.as_os_str().is_empty() {
                ".".to_string()
            } else {
                rel_root.to_string_lossy().replace('\\', "/")
            };
            return Some((value, format!("directory exists (recursive): {}", rel)));
        }
        if depth < max_depth {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        queue.push_back((p, depth + 1));
                    }
                }
            }
        }
    }
    None
}

/// Read a value out of a JSON manifest by a dotted pointer.
///
/// Two shapes are accepted at the pointer:
/// * an object (Composer's `autoload.psr-4`: `{ "app\\": "app/" }`) — the mapped directories become
///   candidates and `pick` decides which one wins;
/// * a plain string (`extra.public-dir: "web"`) — taken as-is.
pub fn resolve_manifest_pointer(
    path: &Path,
    pointer: &str,
    pick: PickStrategy,
    fs: &dyn FileSystem,
) -> Option<(String, String)> {
    let text = fs.read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut cur = &v;
    for seg in pointer.split('.') {
        cur = cur.get(seg)?;
    }
    if let Some(s) = cur.as_str() {
        let value = s.trim().trim_matches('/').to_string();
        if value.is_empty() {
            return None;
        }
        return Some((value, format!("{} {} (string)", path.display(), pointer)));
    }
    let map = cur.as_object()?;
    let mut candidates: Vec<(String, String)> = map
        .iter()
        .filter_map(|(ns, dir)| {
            dir.as_str().map(|d| (d.trim_matches('/').to_string(), ns.clone()))
        })
        .filter(|(d, _)| !d.is_empty())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|(d, _)| d.matches('/').count());
    let chosen = match pick {
        PickStrategy::ShallowestDir | PickStrategy::ByNamespaceKey | PickStrategy::FirstDir => {
            candidates.remove(0)
        }
    };
    Some((
        chosen.0.clone(),
        format!("{} {} (map_dir={}/)", path.display(), pointer, chosen.0),
    ))
}

/// Framework recognition **without code evidence** — the P0 subset of P3's detection.
///
/// Only detectors that read a manifest or probe a file can fire this early (`manifest_dependency` /
/// `lock_dependency` / `file_exists`). `import_exists` / `call_exists` need parsed code, so they are
/// skipped here; a framework that is only recognisable from code is therefore detected in P3, not P0.
/// That asymmetry is acceptable precisely because P0 uses the result only for *exclusions*, which are
/// always fallible in the safe direction (see [`crate::phase::exclude`]).
pub fn detect_without_code(
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    sub_root: &Path,
    project_root: &Path,
    language: &Language,
    techstack: &dyn TechStackRegistry,
) -> Vec<String> {
    let mut hits: Vec<(String, f32)> = Vec::new();
    for fk in kb.all() {
        if fk.language != *language && language.as_str() != Language::UNKNOWN && fk.language.as_str() != "*"
        {
            continue;
        }
        let mut best = 0f32;
        for d in &fk.detectors {
            let ok = match d {
                Detector::ManifestDependency { manifest, dependency, .. } => {
                    manifest_has(&sub_root.join(manifest), dependency, fs, techstack, language)
                        || manifest_has(&project_root.join(manifest), dependency, fs, techstack, language)
                }
                Detector::LockDependency { lock, dependency, .. } => {
                    lock_has(&sub_root.join(lock), dependency, fs, techstack, language)
                        || lock_has(&project_root.join(lock), dependency, fs, techstack, language)
                }
                Detector::FileExists { path, .. } => {
                    sub_root.join(path).exists() || project_root.join(path).exists()
                }
                // No parsed code yet — cannot fire in P0.
                Detector::ImportExists { .. } | Detector::CallExists { .. } => false,
            };
            if ok {
                best = best.max(d.confidence());
            }
        }
        // Knowledge that applies unconditionally to the language (the cross-framework generic layer)
        // contributes even with no detector of its own.
        if best == 0.0 && fk.apply_without_detection {
            hits.push((fk.id.clone(), 0.0));
            continue;
        }
        if best > 0.0 {
            hits.push((fk.id.clone(), best));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    expand_provided(hits, kb).into_iter().map(|(id, _)| id).collect()
}

/// Expand `provides`: recognising a framework recognises the component knowledge it bundles.
///
/// Breadth-first and cycle-safe (`seen`), and **directly detected knowledge always precedes provided
/// knowledge** regardless of confidence — a component may declare a lower confidence than some other
/// framework's direct hit, but "the code / manifest said so" outranks "something else said so". That
/// ordering is what keeps a framework's own declaration of a single-valued field (`db_verbs`,
/// `method_ref` …, all taken by `find_map`) ahead of a component's.
pub fn expand_provided(
    hits: Vec<(String, f32)>,
    kb: &dyn KnowledgeProvider,
) -> Vec<(String, f32)> {
    let mut out = hits;
    let mut seen: HashSet<String> = out.iter().map(|(id, _)| id.clone()).collect();
    let mut i = 0;
    while i < out.len() {
        let (id, conf) = out[i].clone();
        if let Some(fk) = kb.by_id(&id) {
            for p in &fk.provides {
                if seen.insert(p.clone()) {
                    out.push((p.clone(), conf * PROVIDED_CONFIDENCE_DECAY));
                }
            }
        }
        i += 1;
    }
    out
}

/// Confidence granted to knowledge that was merely *provided* rather than detected.
const PROVIDED_CONFIDENCE_DECAY: f32 = 0.9;

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::FrameworkKnowledge;

    #[test]
    fn manifest_pointer_reads_a_plain_string() {
        // A pointer that lands on a scalar (e.g. `extra.public-dir: "web"`) must be usable, not just
        // namespace maps like `autoload.psr-4`.
        let dir = std::env::temp_dir().join(format!("gt-resolve-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("composer.json");
        std::fs::write(&path, r#"{"extra": {"public-dir": "web/"}}"#).unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let got = resolve_manifest_pointer(&path, "extra.public-dir", PickStrategy::FirstDir, &fs);
        assert_eq!(got.map(|(v, _)| v), Some("web".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The i18n path convention is the **stack's**, not the kernel's: `lang/{locale}/*.php` used to be
    /// hard-coded, so a JS project's `src/locales/en/…` got no locale at all (and i18n coverage was
    /// silently wrong). A stack that declares nothing must yield `None`, never a guess.
    #[test]
    fn locale_comes_from_the_stacks_own_path_convention() {
        let ts = gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_php::PhpTechStackAdapter::new()))
            .register(Box::new(gt_adapter_techstack::JsTechStackAdapter::new()));
        let php = Language::new(Language::PHP);
        let js = Language::new(Language::JAVASCRIPT);

        assert_eq!(
            locale_of_path("lang/zh-cn/messages.php", &php, &ts),
            Some("zh-cn".to_string())
        );
        assert_eq!(
            locale_of_path("resources/lang/en/auth.php", &php, &ts),
            Some("en".to_string()),
            "Laravel ≤8 的语言包在 resources/lang/ 下"
        );
        assert_eq!(
            locale_of_path("src/locales/en/translation.json", &js, &ts),
            Some("en".to_string())
        );
        assert_eq!(
            locale_of_path("src/locales/en.json", &js, &ts),
            Some("en".to_string()),
            "一个语言一个文件的布局也要认"
        );
        assert_eq!(
            locale_of_path("src/main/resources/messages.properties", &Language::new(Language::JAVA), &ts),
            None,
            "没有适配器声明约定的栈不猜"
        );
    }

    // ------------------------------------------------------- resolve_directory_exists
    //
    // Shared by every `directory_exists` root rule — `fkb/python/fastapi.yaml` probes `app` and
    // `fkb/java/spring-boot.yaml` probes `src/main/java`. Both feed `app_root`, which in turn feeds the
    // `{app_root}` placeholder of exclude globs, so a wrong value silently mis-aims the whole scan.

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gt-facts-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The value is the **parent** of the marker path — the source root, not the marker directory itself.
    /// A single-module project that hits directly under the root therefore yields `"."`.
    #[test]
    fn directory_exists_returns_the_parent_dir_as_the_source_root() {
        let root = scratch_dir("direct");
        std::fs::create_dir_all(root.join("app")).unwrap();

        let (value, why) = resolve_directory_exists(&root, "app").expect("app 存在，应命中");
        assert_eq!(value, ".", "单模块直命中时源码根就是子工程根");
        assert!(why.contains("directory exists"), "溯源应说明依据，实际：{why}");
    }

    /// Multi-module: the **shallowest** hit module wins (breadth-first), and the value is the module dir —
    /// never the marker path itself.
    #[test]
    fn directory_exists_prefers_the_shallowest_module() {
        let root = scratch_dir("shallow");
        std::fs::create_dir_all(root.join("mall-admin/src/main/java")).unwrap();
        std::fs::create_dir_all(root.join("deep/x/y/src/main/java")).unwrap();

        let (value, _) = resolve_directory_exists(&root, "src/main/java").expect("应有命中");
        assert_eq!(value, "mall-admin", "应返回最浅命中模块的目录，实际：{value}");
    }

    /// The probe is depth-limited (a large repo must not be walked whole); one level past the limit the
    /// marker is not found, which is what sends the caller to its fallback / diagnostic.
    #[test]
    fn directory_exists_is_depth_limited() {
        let hit = scratch_dir("depth-hit");
        std::fs::create_dir_all(hit.join("d1/d2/d3/d4/d5/app")).unwrap();
        assert_eq!(
            resolve_directory_exists(&hit, "app").map(|(v, _)| v),
            Some("d1/d2/d3/d4/d5".to_string()),
            "深度上限内的命中应被找到"
        );

        let miss = scratch_dir("depth-miss");
        std::fs::create_dir_all(miss.join("d1/d2/d3/d4/d5/d6/app")).unwrap();
        assert_eq!(
            resolve_directory_exists(&miss, "app"),
            None,
            "超过深度上限应放弃探测并返回 None"
        );
    }

    /// Nothing found -> `None` (never a guess), so the rule's fallback is what fires.
    #[test]
    fn directory_exists_returns_none_when_the_tree_has_no_marker() {
        let root = scratch_dir("miss");
        std::fs::create_dir_all(root.join("app/models")).unwrap();
        assert_eq!(resolve_directory_exists(&root, "src/main/java"), None);
    }

    /// A relative path with no real segments is not a probe at all.
    #[test]
    fn directory_exists_rejects_an_empty_path() {
        let root = scratch_dir("empty");
        assert_eq!(resolve_directory_exists(&root, ""), None);
        assert_eq!(resolve_directory_exists(&root, "///"), None);
    }

    // ------------------------------------------------------- expand_provided
    //
    // Real FKB uses this (`fkb/php/laravel.yaml: provides: [illuminate-database]`), and the **order** it
    // produces decides which framework's declaration wins for single-valued fields (`db_verbs`,
    // `method_ref` … are taken with `find_map`).

    struct StaticKb(Vec<FrameworkKnowledge>);

    impl KnowledgeProvider for StaticKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.0.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.0.iter().find(|fk| fk.id == id)
        }
    }

    fn fk(id: &str, provides: &[&str]) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: id.to_string(),
            provides: provides.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Directly detected knowledge must stay **ahead** of provided knowledge regardless of confidence.
    #[test]
    fn provided_knowledge_comes_after_every_direct_hit() {
        let kb = StaticKb(vec![fk("framework", &["comp"]), fk("comp", &[]), fk("other", &[])]);
        let got = expand_provided(vec![("framework".into(), 0.5), ("other".into(), 0.4)], &kb);

        let order: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            order,
            vec!["framework", "other", "comp"],
            "直接检出的知识必须全部排在 provided 之前：{order:?}"
        );
    }

    /// Provided confidence decays (0.9x) and does so transitively, so a second-hand component ranks below
    /// a first-hand one.
    #[test]
    fn provided_confidence_decays_and_is_transitive() {
        let kb = StaticKb(vec![fk("a", &["b"]), fk("b", &["c"]), fk("c", &[])]);
        let got = expand_provided(vec![("a".into(), 1.0)], &kb);

        let conf = |id: &str| got.iter().find(|(i, _)| i == id).map(|(_, c)| *c);
        assert_eq!(conf("a"), Some(1.0), "直接检出不衰减");
        assert_eq!(conf("b"), Some(0.9), "一级 provided 衰减一次");
        assert!(
            (conf("c").unwrap_or_default() - 0.81).abs() < 1e-6,
            "二级 provided 应再衰减一次，实际：{:?}",
            conf("c")
        );
        assert_eq!(got.len(), 3, "展开应传递：{got:?}");
    }

    /// A `provides` cycle must terminate and must never duplicate an entry.
    #[test]
    fn provided_cycles_terminate_without_duplicates() {
        let kb = StaticKb(vec![fk("a", &["b"]), fk("b", &["a"])]);
        let got = expand_provided(vec![("a".into(), 1.0)], &kb);

        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "环应终止且不重复：{ids:?}");
    }
}
