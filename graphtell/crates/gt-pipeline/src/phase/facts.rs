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
    if dependency.is_empty() {
        // A blank FKB dependency would be "contained" in every manifest name and in the file text
        // itself, so without this guard a malformed `dependency: ""` detector flags every project
        // that merely has the manifest. Reject like a missing file rather than matching all.
        return false;
    }
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
/// the first hit module's relative dir (e.g. `shop-admin`). Only when the whole tree cannot be found do we
/// return `None` (triggering fallback / a diagnostic).
pub fn resolve_directory_exists(root: &Path, rel: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
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
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()))
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
            "Laravel ≤8 keeps its language packs under resources/lang/"
        );
        assert_eq!(
            locale_of_path("src/locales/en/translation.json", &js, &ts),
            Some("en".to_string())
        );
        assert_eq!(
            locale_of_path("src/locales/en.json", &js, &ts),
            Some("en".to_string()),
            "the one-file-per-locale layout must be recognised too"
        );
        assert_eq!(
            locale_of_path("src/main/resources/messages.properties", &Language::new(Language::JAVA), &ts),
            None,
            "a stack whose adapter declares no convention is not guessed"
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

        let (value, why) = resolve_directory_exists(&root, "app").expect("app exists, so it must hit");
        assert_eq!(value, ".", "on a direct single-module hit the source root is the sub-project root");
        assert!(why.contains("directory exists"), "the provenance must state its evidence, got: {why}");
    }

    /// Multi-module: the **shallowest** hit module wins (breadth-first), and the value is the module dir —
    /// never the marker path itself.
    #[test]
    fn directory_exists_prefers_the_shallowest_module() {
        let root = scratch_dir("shallow");
        std::fs::create_dir_all(root.join("shop-admin/src/main/java")).unwrap();
        std::fs::create_dir_all(root.join("deep/x/y/src/main/java")).unwrap();

        let (value, _) = resolve_directory_exists(&root, "src/main/java").expect("there must be a hit");
        assert_eq!(value, "shop-admin", "must return the directory of the shallowest matching module, got: {value}");
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
            "a hit within the depth limit must be found"
        );

        let miss = scratch_dir("depth-miss");
        std::fs::create_dir_all(miss.join("d1/d2/d3/d4/d5/d6/app")).unwrap();
        assert_eq!(
            resolve_directory_exists(&miss, "app"),
            None,
            "past the depth limit it must give up probing and return None"
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
            "directly detected knowledge must all rank before provided ones: {order:?}"
        );
    }

    /// Provided confidence decays (0.9x) and does so transitively, so a second-hand component ranks below
    /// a first-hand one.
    #[test]
    fn provided_confidence_decays_and_is_transitive() {
        let kb = StaticKb(vec![fk("a", &["b"]), fk("b", &["c"]), fk("c", &[])]);
        let got = expand_provided(vec![("a".into(), 1.0)], &kb);

        let conf = |id: &str| got.iter().find(|(i, _)| i == id).map(|(_, c)| *c);
        assert_eq!(conf("a"), Some(1.0), "a direct detection does not decay");
        assert_eq!(conf("b"), Some(0.9), "a first-level provided decays once");
        assert!(
            (conf("c").unwrap_or_default() - 0.81).abs() < 1e-6,
            "a second-level provided must decay once more, got: {:?}",
            conf("c")
        );
        assert_eq!(got.len(), 3, "expansion must be transitive: {got:?}");
    }

    /// A `provides` cycle must terminate and must never duplicate an entry.
    #[test]
    fn provided_cycles_terminate_without_duplicates() {
        let kb = StaticKb(vec![fk("a", &["b"]), fk("b", &["a"])]);
        let got = expand_provided(vec![("a".into(), 1.0)], &kb);

        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "a cycle must terminate without duplicates: {ids:?}");
    }

    // ---- under-covered branch tests ----
    // `resolve_root_source` / `detect_without_code` / `manifest_has` are exercised *indirectly* via
    // `exclude::resolve_for_sub`, but the three branches below never get a direct, isolated pin.

    /// `resolve_manifest_pointer` must pick the **shallowest** directory when the pointer lands on a
    /// namespace map (psr-4 style): that value becomes `app_root`, and a wrong pick silently mis-aims the
    /// whole scan. `ShallowestDir` / `ByNamespaceKey` / `FirstDir` currently share the exact "take the
    /// first (shallowest) candidate" behaviour — pinned here as the documented status quo.
    #[test]
    fn manifest_pointer_picks_shallowest_dir_from_a_map() {
        let dir = std::env::temp_dir().join(format!("gt-facts-map-{}-{}", std::process::id(), "x"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("composer.json");
        std::fs::write(
            &path,
            r#"{"autoload": {"psr-4": {"app\\": "app/", "admin\\": "a/b/c/"}}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();

        assert_eq!(
            resolve_manifest_pointer(&path, "autoload.psr-4", PickStrategy::ShallowestDir, &fs)
                .map(|(v, _)| v),
            Some("app".to_string()),
            "the shallowest candidate app/ must win, not a/b/c/"
        );
        assert_eq!(
            resolve_manifest_pointer(&path, "autoload.psr-4", PickStrategy::ByNamespaceKey, &fs)
                .map(|(v, _)| v),
            Some("app".to_string()),
            "ByNamespaceKey currently behaves like ShallowestDir (takes the shallowest)"
        );
        // A map whose directories all trim to empty yields no candidate.
        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{"autoload": {"psr-4": {"x\\": "/"}}}"#).unwrap();
        assert!(
            resolve_manifest_pointer(&empty, "autoload.psr-4", PickStrategy::FirstDir, &fs).is_none(),
            "a directory trimmed to empty yields no candidate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `detect_without_code` must apply the language gate: a PHP framework is detected for `php` (and for
    /// `UNKNOWN`, where the gate lifts) but NOT for `javascript`. Breaking the gate would silently
    /// mis-detect frameworks across languages.
    #[test]
    fn detect_without_code_respects_language_gate() {
        let kb = StaticKb(vec![FrameworkKnowledge {
            id: "tp".into(),
            language: Language::new("php"),
            detectors: vec![Detector::ManifestDependency {
                manifest: "composer.json".into(),
                dependency: "topthink/framework".into(),
                confidence: 0.95,
            }],
            ..Default::default()
        }]);
        let root = scratch_dir("detect-gate");
        std::fs::write(
            root.join("composer.json"),
            r#"{"require": {"topthink/framework": "^6"}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        // Empty registry -> `manifest_has` falls back to a plain-text probe, which still matches.
        let ts = gt_domain::port::DefaultTechStackRegistry::new();

        let php = detect_without_code(&kb, &fs, &root, &root, &Language::new("php"), &ts);
        assert!(php.contains(&"tp".to_string()), "php must detect tp, got: {php:?}");

        let js = detect_without_code(&kb, &fs, &root, &root, &Language::new("javascript"), &ts);
        assert!(!js.contains(&"tp".to_string()), "javascript must not detect a php framework, got: {js:?}");

        let unk = detect_without_code(
            &kb,
            &fs,
            &root,
            &root,
            &Language::new(Language::UNKNOWN),
            &ts,
        );
        assert!(unk.contains(&"tp".to_string()), "UNKNOWN must lift the language gate, got: {unk:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no tech-stack adapter (empty registry), `manifest_has` must still match via the conservative
    /// whole-file text probe, and must return `false` for a missing file or an absent dependency.
    #[test]
    fn manifest_has_falls_back_to_plain_text_probe() {
        let root = scratch_dir("mh-fallback");
        std::fs::write(
            root.join("composer.json"),
            r#"{"require": {"topthink/framework": "^6"}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let php = Language::new("php");

        assert!(
            manifest_has(&root.join("composer.json"), "topthink/framework", &fs, &ts, &php),
            "a manifest containing the dependency string must match (degraded to plain-text matching)"
        );
        assert!(
            !manifest_has(&root.join("composer.json"), "laravel/framework", &fs, &ts, &php),
            "a dependency string that is absent must not match"
        );
        assert!(
            !manifest_has(&root.join("missing.json"), "topthink/framework", &fs, &ts, &php),
            "a missing file must simply be false"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `lock_has` shares `declared_in` with `manifest_has` but was never exercised directly. With no adapter
    /// it must still match via the conservative whole-file text probe, and must be `false` for an absent
    /// dependency or a missing lock file.
    #[test]
    fn lock_has_falls_back_to_plain_text_probe() {
        let root = scratch_dir("lh-fallback");
        std::fs::write(
            root.join("composer.lock"),
            r#"{"packages":[{"name":"topthink/framework","version":"^6"}]}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let php = Language::new("php");

        assert!(
            lock_has(&root.join("composer.lock"), "topthink/framework", &fs, &ts, &php),
            "a lock file containing the dependency string must match (degraded to plain-text matching)"
        );
        assert!(
            !lock_has(&root.join("composer.lock"), "laravel/framework", &fs, &ts, &php),
            "a dependency string absent from the lock file must not match"
        );
        assert!(
            !lock_has(&root.join("missing.lock"), "topthink/framework", &fs, &ts, &php),
            "a missing lock file must simply be false"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `resolve_root_source` is only exercised *through* `exclude::resolve_for_sub` (where sub_root ==
    /// project_root), so its `project_root` fallback and its `Manifest` / `ManifestEntries` arms are never
    /// pinned directly. This isolates all four arms.
    #[test]
    fn resolve_root_source_covers_each_kind_and_project_root_fallback() {
        let sub = scratch_dir("rs-sub"); // no manifest here
        let proj = scratch_dir("rs-proj");
        std::fs::write(
            proj.join("composer.json"),
            r#"{"autoload": {"psr-4": {"app\\": "app/"}}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let sub_proj = provisional_sub(&sub, &Language::new("php"));

        // ManifestJson resolves via the *project root* because the sub-root has no copy of the manifest
        // — the `else { project_root.join(manifest) }` fallback branch.
        let got = resolve_root_source(
            &RootSource::ManifestJson {
                manifest: "composer.json".into(),
                pointer: "autoload.psr-4".into(),
                pick: PickStrategy::ShallowestDir,
            },
            &sub,
            &proj,
            &sub_proj,
            &fs,
            &parsers,
            &ts,
        );
        assert_eq!(got.map(|(v, _)| v), Some("app".to_string()), "the project_root fallback must resolve to app");

        // DirectoryExists: a marker dir present directly under the root yields "." as the source root.
        let dir_root = scratch_dir("rs-dir");
        std::fs::create_dir_all(dir_root.join("app")).unwrap();
        let got = resolve_root_source(
            &RootSource::DirectoryExists { path: "app".into() },
            &dir_root,
            &dir_root,
            &provisional_sub(&dir_root, &Language::new("php")),
            &fs,
            &parsers,
            &ts,
        );
        assert_eq!(got.map(|(v, _)| v), Some(".".to_string()));

        // Manifest arm: with no adapter for the language, `read_manifest` is never called -> None.
        assert!(
            resolve_root_source(
                &RootSource::Manifest {
                    manifest: "composer.json".into(),
                    pointer: "autoload.psr-4".into(),
                },
                &sub,
                &proj,
                &provisional_sub(&sub, &Language::new("cobol")),
                &fs,
                &parsers,
                &ts,
            )
            .is_none(),
            "a Manifest source with no adapter must return None"
        );

        // ManifestEntries arm: always resolves to None from a single-value source.
        assert!(
            resolve_root_source(
                &RootSource::ManifestEntries {
                    manifest: "composer.json".into(),
                    root: "connections".into(),
                    fields: vec![],
                    default_from: None,
                },
                &sub,
                &proj,
                &sub_proj,
                &fs,
                &parsers,
                &ts,
            )
            .is_none(),
            "ManifestEntries must return None through resolve_root_source"
        );
        let _ = std::fs::remove_dir_all(&sub);
        let _ = std::fs::remove_dir_all(&proj);
        let _ = std::fs::remove_dir_all(&dir_root);
    }

    /// `resolve_root_entries` is a distinct public entry that `resolve_root_source` deliberately does not
    /// cover (it returns a list, not one value). Pin its two `None` branches: a non-`ManifestEntries`
    /// source, and a `ManifestEntries` source with no adapter able to read it.
    #[test]
    fn resolve_root_entries_returns_none_outside_manifest_entries_and_without_adapter() {
        let root = scratch_dir("rre");
        let fs = gt_adapter_fs::StdFileSystem::new();
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();

        // A single-value source is rejected up front by the `let ... else`.
        assert!(resolve_root_entries(
            &RootSource::DirectoryExists { path: "app".into() },
            &root,
            &provisional_sub(&root, &Language::new("php")),
            &fs,
            &parsers,
            &ts,
        )
        .is_none());

        // A `ManifestEntries` source with no adapter for the language resolves to None.
        assert!(resolve_root_entries(
            &RootSource::ManifestEntries {
                manifest: "config/database.php".into(),
                root: "connections".into(),
                fields: vec![],
                default_from: None,
            },
            &root,
            &provisional_sub(&root, &Language::new("cobol")),
            &fs,
            &parsers,
            &ts,
        )
        .is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `provisional_sub` is the stand-in P0 uses for every resolution; pin that it stamps the root path,
    /// language, and the documented sentinel id/role (adapters read these to locate manifests).
    #[test]
    fn provisional_sub_carries_root_language_and_sentinels() {
        let root = scratch_dir("ps");
        let sub = provisional_sub(&root, &Language::new("php"));
        assert_eq!(sub.id, SubProjectId(0));
        assert_eq!(sub.project_id, ProjectId(0));
        assert_eq!(sub.root_path, root);
        assert_eq!(sub.language, Language::new("php"));
        assert_eq!(sub.role, "unknown");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `detect_without_code` branches not covered by the language-gate test:
    /// * `apply_without_detection` — a framework with no matching detector still contributes (the safe,
    ///   language-wide generic layer);
    /// * `language: "*"` — a universal framework fires for *any* language, not just its own;
    /// * the `FileExists` detector (vs the `ManifestDependency` one already pinned).
    #[test]
    fn detect_without_code_applies_without_detection_universal_and_file_exists() {
        let root = scratch_dir("dwc-extra");
        std::fs::write(root.join("composer.json"), r#"{"require": {"topthink/framework": "^6"}}"#).unwrap();
        std::fs::write(root.join("artisan"), "").unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();

        let kb = StaticKb(vec![
            FrameworkKnowledge {
                id: "generic".into(),
                language: Language::new("php"),
                apply_without_detection: true,
                ..Default::default()
            },
            FrameworkKnowledge {
                id: "universal".into(),
                language: Language::new("*"),
                detectors: vec![Detector::ManifestDependency {
                    manifest: "composer.json".into(),
                    dependency: "topthink/framework".into(),
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            FrameworkKnowledge {
                id: "artisan-fw".into(),
                language: Language::new("php"),
                detectors: vec![Detector::FileExists { path: "artisan".into(), confidence: 0.8 }],
                ..Default::default()
            },
        ]);

        // Queried as php: the no-detector generic (apply_without_detection), the universal (*) framework,
        // and the FileExists detector for artisan-fw must all fire.
        let php = detect_without_code(&kb, &fs, &root, &root, &Language::new("php"), &ts);
        assert!(php.contains(&"generic".to_string()), "apply_without_detection must hit even with no detector: {php:?}");
        assert!(php.contains(&"universal".to_string()), "the generic (*) framework must hit under php: {php:?}");
        assert!(php.contains(&"artisan-fw".to_string()), "the FileExists detector must hit: {php:?}");

        // Queried as javascript: only the universal framework survives the language gate; the two php-only
        // frameworks are skipped.
        let js = detect_without_code(&kb, &fs, &root, &root, &Language::new("javascript"), &ts);
        assert!(js.contains(&"universal".to_string()), "the generic (*) must hit for any language: {js:?}");
        assert!(!js.contains(&"generic".to_string()), "a php-only generic must not hit under javascript: {js:?}");
        assert!(!js.contains(&"artisan-fw".to_string()), "the php-only artisan-fw must not hit under javascript: {js:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `manifest_has` with a real tech-stack adapter must resolve via the adapter's parsed dependency
    /// names (the empty-registry text probe is already pinned separately). This exercises the
    /// `techstack.adapter_for` + `names_of` branch and its `n == needle || n.contains(needle)` match.
    #[test]
    fn manifest_has_uses_adapter_parsed_dependencies() {
        let root = scratch_dir("mh-adapter");
        std::fs::write(
            root.join("composer.json"),
            r#"{"require": {"topthink/framework": "^6"}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()));
        let php = Language::new("php");

        assert!(
            manifest_has(&root.join("composer.json"), "topthink/framework", &fs, &ts, &php),
            "a dependency name resolved by the adapter must match"
        );
        // `framework` is a *substring* of the declared name `topthink/framework`, exercising the
        // `n.contains(needle)` arm rather than the exact-`==` arm.
        assert!(
            manifest_has(&root.join("composer.json"), "framework", &fs, &ts, &php),
            "a declared name containing the needle must match as well (the contains branch)"
        );
        assert!(
            !manifest_has(&root.join("composer.json"), "symfony/console", &fs, &ts, &php),
            "a dependency in neither the adapter nor the text must not match"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ===================================================================
    // Negative coverage
    //
    // The tests above pin the resolving paths. Below: everything that must resolve to **nothing**
    // (and must not panic) — unreadable files, malformed manifests, non-scalar pointer targets,
    // markers that are files rather than directories, detectors that cannot fire before parsing.
    // Every one of these degrades in the direction "fall through to the caller's fallback".
    // ===================================================================

    /// A file that exists but cannot be read: `declared_in` must stop at the read failure rather than
    /// panicking or probing empty text. Uses an injected fs because the real one only fails by absence.
    struct UnreadableFs;

    impl FileSystem for UnreadableFs {
        fn exists(&self, _: &Path) -> bool {
            true
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &Path) -> gt_domain::error::Result<String> {
            Err(gt_domain::error::DomainError::infra("unreadable"))
        }
        fn len(&self, _: &Path) -> gt_domain::error::Result<u64> {
            Err(gt_domain::error::DomainError::infra("unreadable"))
        }
    }

    #[test]
    fn declared_in_is_false_when_the_file_exists_but_cannot_be_read() {
        let path = PathBuf::from("app/composer.json");
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let php = Language::new("php");
        assert!(
            !manifest_has(&path, "topthink/framework", &UnreadableFs, &ts, &php),
            "an unreadable manifest must be false, not panic"
        );
        assert!(
            !lock_has(&path, "topthink/framework", &UnreadableFs, &ts, &php),
            "the same holds for a lock file"
        );
    }

    /// A blank dependency must match **nothing**: an empty needle is "contained" in every name *and* in
    /// every text, so without a guard a malformed FKB field would claim every project that merely has the
    /// manifest. `declared_in` now rejects an empty dependency up front (hard false).
    #[test]
    fn empty_dependency_matches_nothing() {
        let root = scratch_dir("empty-dep");
        std::fs::write(root.join("composer.json"), r#"{"require": {}}"#).unwrap();
        std::fs::write(root.join("composer.lock"), r#"{"packages": []}"#).unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let php = Language::new("php");

        assert!(
            !manifest_has(&root.join("composer.json"), "", &fs, &ts, &php),
            "a blank dependency must match nothing"
        );
        assert!(!lock_has(&root.join("composer.lock"), "", &fs, &ts, &php));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn locale_is_none_when_the_path_does_not_follow_the_convention() {
        let ts = gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()));
        let php = Language::new("php");
        assert_eq!(
            locale_of_path("app/controller/Index.php", &php, &ts),
            None,
            "a stack with conventions still yields None when nothing matches — never a guess"
        );
        // An empty path segment (`lang//`) must be rejected as "no locale" — capturing an empty locale
        // would stamp a meaningless value instead of falling through to the caller's fallback.
        assert_eq!(
            locale_of_path("lang//messages.php", &php, &ts),
            None,
            "an empty locale segment must be rejected, not stamped as empty"
        );
    }

    /// A *file* that happens to be named like the marker is not a source root: only directories count.
    #[test]
    fn directory_exists_ignores_a_file_named_like_the_marker() {
        let root = scratch_dir("file-marker");
        std::fs::write(root.join("app"), "not a directory").unwrap();
        assert_eq!(
            resolve_directory_exists(&root, "app"),
            None,
            "a plain file must not be taken for the source root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_exists_collapses_repeated_slashes_and_rejects_dot() {
        let root = scratch_dir("slashes");
        std::fs::create_dir_all(root.join("src/main")).unwrap();
        assert_eq!(
            resolve_directory_exists(&root, "src//main").map(|(v, _)| v),
            Some(".".to_string()),
            "repeated slashes must not add empty path segments"
        );
        // `.` is rejected like the empty path: a rule probing `.` would trivially always hit (`<root>/."`
        // is always a directory) and silently stamp the sub-project root as `app_root` — so it must
        // yield `None` (fall through to the caller's fallback) instead.
        assert_eq!(
            resolve_directory_exists(&root, "."),
            None,
            "`.` must be rejected like the empty path is"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_pointer_is_none_for_missing_or_malformed_files() {
        let fs = gt_adapter_fs::StdFileSystem::new();
        let missing = PathBuf::from("/definitely/not/here/composer.json");
        assert!(
            resolve_manifest_pointer(&missing, "a.b", PickStrategy::FirstDir, &fs).is_none(),
            "a missing manifest degrades to None"
        );

        let root = scratch_dir("bad-json");
        let bad = root.join("composer.json");
        std::fs::write(&bad, "{not json at all").unwrap();
        assert!(
            resolve_manifest_pointer(&bad, "a.b", PickStrategy::FirstDir, &fs).is_none(),
            "malformed JSON degrades to None"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_pointer_is_none_when_the_pointer_misses() {
        let root = scratch_dir("miss-pointer");
        let path = root.join("composer.json");
        std::fs::write(&path, r#"{"extra": {"public-dir": "web"}}"#).unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();

        assert!(
            resolve_manifest_pointer(&path, "extra.nope", PickStrategy::FirstDir, &fs).is_none(),
            "an absent key must yield None"
        );
        assert!(
            resolve_manifest_pointer(&path, "extra.public-dir.deeper", PickStrategy::FirstDir, &fs)
                .is_none(),
            "walking past a scalar must yield None"
        );
        assert!(
            resolve_manifest_pointer(&path, "", PickStrategy::FirstDir, &fs).is_none(),
            "an empty pointer has no segment to walk"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pointer may land on something that is neither a string nor a map (a list, a number, a bool,
    /// null) — none of those carry a directory, and none may panic.
    #[test]
    fn manifest_pointer_rejects_scalarless_targets() {
        let root = scratch_dir("scalarless");
        let path = root.join("composer.json");
        std::fs::write(&path, r#"{"a": [1, 2], "b": 3, "c": true, "d": null}"#).unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();

        for pointer in ["a", "b", "c", "d"] {
            assert!(
                resolve_manifest_pointer(&path, pointer, PickStrategy::FirstDir, &fs).is_none(),
                "a non-string, non-object target must yield None: `{pointer}`"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Inside a namespace map, only string values are directory candidates; anything else is ignored
    /// rather than poisoning the pick.
    #[test]
    fn manifest_pointer_ignores_non_string_values_in_a_map() {
        let root = scratch_dir("map-values");
        let path = root.join("composer.json");
        std::fs::write(
            &path,
            r#"{"autoload": {"psr-4": {"a\\": 42, "b\\": {"x": 1}, "c\\": "app/"}}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();

        assert_eq!(
            resolve_manifest_pointer(&path, "autoload.psr-4", PickStrategy::ShallowestDir, &fs)
                .map(|(v, _)| v),
            Some("app".to_string()),
            "only the string-valued entry is a candidate"
        );

        // Every entry non-string -> no candidate at all.
        let none_path = root.join("none.json");
        std::fs::write(&none_path, r#"{"autoload": {"psr-4": {"a\\": 1, "b\\": true}}}"#).unwrap();
        assert!(
            resolve_manifest_pointer(&none_path, "autoload.psr-4", PickStrategy::FirstDir, &fs)
                .is_none(),
            "a map with no string values yields no candidate"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Code-evidence detectors (`import_exists` / `call_exists`) need parsed code, so in P0 they must
    /// stay silent — a framework recognisable only from code is detected in P3, never here.
    #[test]
    fn code_only_detectors_cannot_fire_without_parsed_code() {
        let root = scratch_dir("code-only");
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let kb = StaticKb(vec![
            FrameworkKnowledge {
                id: "import-fw".into(),
                language: Language::new("php"),
                detectors: vec![Detector::ImportExists {
                    symbol: "GuzzleHttp".into(),
                    confidence: 0.6,
                }],
                ..Default::default()
            },
            FrameworkKnowledge {
                id: "call-fw".into(),
                language: Language::new("php"),
                detectors: vec![Detector::CallExists {
                    callee: "Db::query".into(),
                    confidence: 0.8,
                }],
                ..Default::default()
            },
        ]);
        let got = detect_without_code(&kb, &fs, &root, &root, &Language::new("php"), &ts);
        assert!(
            got.is_empty(),
            "import/call detectors must not fire before any code is parsed, got: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Detections are returned **highest confidence first**: downstream, single-valued facts (`db_verbs`,
    /// `method_ref` …) are taken with `find_map`, so the order decides which declaration wins.
    #[test]
    fn detections_are_ranked_by_confidence() {
        let root = scratch_dir("rank");
        std::fs::write(
            root.join("composer.json"),
            r#"{"require": {"a/pkg": "^1", "b/pkg": "^1"}}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let kb = StaticKb(vec![
            FrameworkKnowledge {
                id: "low".into(),
                language: Language::new("php"),
                detectors: vec![Detector::ManifestDependency {
                    manifest: "composer.json".into(),
                    dependency: "a/pkg".into(),
                    confidence: 0.6,
                }],
                ..Default::default()
            },
            FrameworkKnowledge {
                id: "high".into(),
                language: Language::new("php"),
                detectors: vec![Detector::ManifestDependency {
                    manifest: "composer.json".into(),
                    dependency: "b/pkg".into(),
                    confidence: 0.95,
                }],
                ..Default::default()
            },
        ]);
        let got = detect_without_code(&kb, &fs, &root, &root, &Language::new("php"), &ts);
        assert_eq!(
            got,
            vec!["high".to_string(), "low".to_string()],
            "the more confident detection must come first: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `lock_dependency` detector is dispatched like `manifest_dependency`, but reads the installed
    /// closure — a package the project never declared itself must still be detected.
    #[test]
    fn lock_dependency_detector_fires_on_installed_packages() {
        let root = scratch_dir("lock-detect");
        std::fs::write(
            root.join("composer.lock"),
            r#"{"packages":[{"name":"topthink/framework"}]}"#,
        )
        .unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let ts = gt_domain::port::DefaultTechStackRegistry::new();
        let kb = StaticKb(vec![FrameworkKnowledge {
            id: "tp-lock".into(),
            language: Language::new("php"),
            detectors: vec![Detector::LockDependency {
                lock: "composer.lock".into(),
                dependency: "topthink/framework".into(),
                confidence: 0.95,
            }],
            ..Default::default()
        }]);

        let got = detect_without_code(&kb, &fs, &root, &root, &Language::new("php"), &ts);
        assert!(got.contains(&"tp-lock".to_string()), "a lock-file dependency must be detected: {got:?}");

        // And a package that is in neither the lock nor anywhere else stays undetected.
        let other = StaticKb(vec![FrameworkKnowledge {
            id: "absent".into(),
            language: Language::new("php"),
            detectors: vec![Detector::LockDependency {
                lock: "composer.lock".into(),
                dependency: "symfony/console".into(),
                confidence: 0.95,
            }],
            ..Default::default()
        }]);
        let got = detect_without_code(&other, &fs, &root, &root, &Language::new("php"), &ts);
        assert!(got.is_empty(), "an absent lock dependency must not be detected: {got:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `provides` entry naming knowledge that is **not in the KB** is still returned: expansion only
    /// needs it to expand further, and callers must tolerate a dangling id (`exclude.rs` skips it
    /// silently). Pinned so nobody "cleans up" the list and changes downstream ordering semantics.
    #[test]
    fn expand_provided_keeps_a_dangling_provided_id() {
        let kb = StaticKb(vec![fk("a", &["ghost"])]);
        let got = expand_provided(vec![("a".into(), 1.0)], &kb);
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a", "ghost"],
            "an unknown provided id stays in the list; readers skip unknown ids themselves: {ids:?}"
        );
    }
}
